# Ferry dashboard

[中文](dashboard.zh-TW.md)

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

 compared   10,000 entries  2,100/s   to send   8,000 files
 sent        4,620 files      340/s   failed        0

 history                 the last 2 minutes   t: the whole transfer
   ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣶⣦⣴⣶⣷⣷⣼⣾⣶⣶⣿⣿⣾⣦⣷⣷⣿⣾⣴⣶⣶⣷⣼⠀⠀⠀⠀⠀⣦⣷⣴⣼⣤⣿⣾⣷⣿⣷⣴⣷⣷⣦⣷⣿⣷⣿⣾⣤  40.0 MiB/s
   ⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣤⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⠀⠀⢀⣸⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣄⡀⠀⠀⡄⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⠀⣰⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ────────────────────────────────────────────────────────────
   ⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠉⠁⠉⠉⠉⠉⠉⠁⠁⠉⠉⠉⠉⠈⠉⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠏⠙⠹⠿⠙⠉⠿⠏⠙⠟⠹⠏⠹⠻⠹
   ⠀⠀⠀⠀⠀⠻⠏⠉⠋⠿⣿⠙⠹⠏⠟⢻⠻⢿⠋⠹⡿⢿⠋⢿⠋⠉⠙⠹⡟⢻⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀  800 files/s
   -02:00         -01:30         -01:00         -00:30       now

 transferring                              size          rate     ETA
   photos/2024/08/IMG_4211.jpg     45%    1.2 GiB    10.0 MiB/s   01:02
   photos/2024/08/IMG_4210.jpg    100%    3.1 MiB             —       —

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s    12 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 pipeline   read ▮▮▯▯  compress ▮▯▯▯  network ▮▮▮▮  decompress ▮▯▯▯  write ▮▯▯▯
 bottleneck: network          compression level 1, 41% saved

 queues                                 now   room
   Source       read                      3     64  ▮▯▯▯
                compress                  2     16  ▮▯▯▯
                data                    257    256  ▮▮▮▮
                send ×4                  34     32  ▮▮▮▮
   client       from Source             256    256  ▮▮▮▮
                to Destination ×4         9     32  ▮▯▯▯
   Destination  receive                   0    256  ▯▯▯▯
                decompress                0     16  ▯▯▯▯
                write                     1     32  ▯▯▯▯
   back         Destination send 0/32 · Source receive 0/256
   messages     Source 0 · Destination 0
   manifest     Source 1.0 MiB/8.0 MiB

 warnings and failures
   cannot read sub/locked.txt: permission denied
```

## What it shows

**Decided.**

- **The whole transfer:** a bar, the bytes sent out of the bytes to send,
  the rate, and the time left.
- **The counts:** entries compared, files to send, files sent, and files
  that failed; beside the entries compared and the files sent, how many a
  second, over the last 5 seconds, so that a tree of many small files shows
  its pace where bytes a second say little.
- **Each SSH connection:** its rate, and how long ago its last
  [heartbeat](architecture.md#connections) came, grouped by host. A
  location on this machine has none.
- **The pipeline:** how full the queue before each stage is, from reading
  to writing, and which stage is the bottleneck.
- **The queues:** as each side's latest second left them, those the data
  goes through in a table, in the order it goes, from the Source through the
  client to the Destination: what waits in each, its room, and cells as
  full as it is; a connection's paths together, as `send ×4`. Then, on a
  line each, those used of the needs and results
  going back, the messages, and the manifest. A queue is counted as things
  go into it, so one that shows more than its room has someone waiting to
  put more in; see [profiling](profiling.md#following-a-transfer-as-it-goes),
  and `--profile-file` for each path's own, second by second.
- **Warnings and failures:** the latest few, as they come.
- **The files being transferred,** as rclone shows them: each file the
  Destination is receiving or writing, by its path relative to the
  destination, with how much of it has come, its size, its rate, and its
  time left. Those that started first come first, so that a large file
  stays in place while small ones come and go; the first 6 are shown, and
  a count of the rest. A path too long for the screen loses its middle to
  `…`. Unlike rclone's, whose files come in parallel, the record stream
  brings one file's bytes at a time: most often one file moves while those
  before it, all here, wait for their writers.
- **A history,** as nvitop draws one: a second to a column of braille
  dots, over the last 2 minutes, with now on the right; `t` switches to the
  whole transfer, each column then the average of the seconds it covers,
  and back. Above the axis, the bytes a second the bar counts; below it,
  mirrored, the files a second whose content arrived, so that a tree of
  many small files, whose bytes say little, shows its pace too. Each half
  scales to the most it shows. A short screen leaves it out, as
  [its layout](#layout) says. The client keeps what came in each
  second from the counts it already has; nothing more comes over the
  connections.

## Layout

**Decided.** Each section is as tall as the screen makes it, never as what
it holds: a screen of one size keeps one layout from start to end, only its
numbers changing.

| Section | Rows |
|---|---|
| The title and the bar, and a blank row | 3 |
| The counts, and a blank row | 3 |
| The history: its heading, 4 rows above its axis, the axis, 3 below, the times, and a blank row | 11 |
| The files being transferred: a heading, 6 files, `… and N more`, and a blank row | 9 |
| Each host's SSH connections: a heading, one per connection, and a blank row | 2 + its connections |
| The pipeline and the bottleneck, and a blank row | 3 |
| The queues: a heading, the 9 stages, the 3 lines after, and a blank row | 14 |
| Warnings and failures: a heading, and the latest 6 | 7 |

What has less to show leaves its rows blank: fewer files than 6, fewer
warnings, no history yet. The queues show from the start, `—` for what has
not come. A host's connections are known before the dashboard opens, so
its section does not change either.

All of it takes 50 rows, and 6 more for a host with 4 connections, as in
the example above. A shorter screen leaves sections out, whole, in this
order, by its height alone:

1. The queues.
2. The pipeline.
3. The files being transferred, down to 3.
4. The history.
5. Then the warnings, and last the connections, lose their bottom rows.

With a host of 4 connections, that is every section from 56 rows; no
queues from 42; no pipeline either from 39; 3 files being transferred
from 36; and no history from 25. Without a host, each is 6 rows fewer.

At 40 rows:

```text
 ferry: ./data → ntu:/backup                                     00:42 elapsed
 ████████████████░░░░░░░░░░░░  58%   612 MiB of 1.0 GiB   ETA 00:31

 compared   10,000 entries  2,100/s   to send   8,000 files
 sent        4,620 files      340/s   failed        0

 history                 the last 2 minutes   t: the whole transfer
   ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣶⣦⣴⣶⣷⣷⣼⣾⣶⣶⣿⣿⣾⣦⣷⣷⣿⣾⣴⣶⣶⣷⣼⠀⠀⠀⠀⠀⣦⣷⣴⣼⣤⣿⣾⣷⣿⣷⣴⣷⣷⣦⣷⣿⣷⣿⣾⣤  40.0 MiB/s
   ⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣤⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⠀⠀⢀⣸⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣄⡀⠀⠀⡄⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⠀⣰⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ────────────────────────────────────────────────────────────
   ⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠉⠁⠉⠉⠉⠉⠉⠁⠁⠉⠉⠉⠉⠈⠉⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿
   ⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠏⠙⠹⠿⠙⠉⠿⠏⠙⠟⠹⠏⠹⠻⠹
   ⠀⠀⠀⠀⠀⠻⠏⠉⠋⠿⣿⠙⠹⠏⠟⢻⠻⢿⠋⠹⡿⢿⠋⢿⠋⠉⠙⠹⡟⢻⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀  800 files/s
   -02:00         -01:30         -01:00         -00:30       now

 transferring                              size          rate     ETA
   photos/2024/08/IMG_4211.jpg     45%    1.2 GiB    10.0 MiB/s   01:02
   photos/2024/08/IMG_4210.jpg    100%    3.1 MiB             —       —
   photos/2024/08/IMG_4209.jpg    100%    2.9 MiB             —       —
   photos/2024/08/IMG_4208.jpg    100%    3.4 MiB             —       —
   photos/2024/08/IMG_4207.jpg    100%    3.0 MiB             —       —
   photos/2024/08/IMG_4206.jpg    100%    2.8 MiB             —       —
   … and 37 more

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s    12 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 warnings and failures
   cannot read sub/locked.txt: permission denied






```

At 30 rows, the bottom 5 left blank:

```text
 ferry: ./data → ntu:/backup                                     00:42 elapsed
 ████████████████░░░░░░░░░░░░  58%   612 MiB of 1.0 GiB   ETA 00:31

 compared   10,000 entries  2,100/s   to send   8,000 files
 sent        4,620 files      340/s   failed        0

 transferring                              size          rate     ETA
   photos/2024/08/IMG_4211.jpg     45%    1.2 GiB    10.0 MiB/s   01:02
   photos/2024/08/IMG_4210.jpg    100%    3.1 MiB             —       —
   photos/2024/08/IMG_4209.jpg    100%    2.9 MiB             —       —
   … and 40 more

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s    12 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 warnings and failures
   cannot read sub/locked.txt: permission denied










```

## Colors

**Decided.**

![The dashboard in color: the bar green; the history's bytes a second cyan, above its axis, and its files a second magenta, below; each queue's cells green under half full, yellow to three quarters, red beyond; the bottleneck yellow; a heartbeat that missed one yellow; warnings yellow; the rest as now, its labels dim](../assets/dashboard-colors.svg)

Most of the screen stays as it is: the text in the terminal's own color,
and what says what a number is, dim. Color goes only where it tells you
something at a glance:

- **What needs a look,** as a traffic light: each queue's cells, and the
  pipeline's, green under half full, yellow to three quarters, red beyond,
  so that where the data piles up shows from across the room; the
  bottleneck, if there is one, yellow; a heartbeat yellow once one is
  missed, after 10 s, and red after 20 s, nearing the 30 s at which its
  path is cut; warnings and failures yellow, and the count of failed
  files red once there are any.
- **What is which:** the bar green; in the history, the bytes a second
  cyan, and the files a second magenta, each with its scale in its color.
- **The terminal's own colors,** the 16 named ones, never fixed shades: the
  theme picks them, so that they read on a dark background and a light one.
- **None with `NO_COLOR`** set, as is the convention; dim and bold stay.

## How it behaves

**Decided.**

- **On by default.** Where standard error is a terminal, a transfer shows
  the dashboard. `--no-ui` shows the progress line instead, as
  before the dashboard.
- **A full screen of its own.** The dashboard takes the terminal's
  alternate screen. When the transfer ends, however it ends, the terminal
  is put back as it was, and what stays is the summary line, then every
  warning and failure, written to standard error, so that they stay in the
  scrollback.
- **Nothing is printed over it.** Warnings and failures that would go to
  standard error during the transfer are shown in the dashboard, and
  written out when it closes.
- **A dry run has one too.** In place of the bar, it shows what holds the
  run back, the side whose threads work the most, and at what; in place of
  the files sent and being transferred, how many entries it listed, and the
  latest few; and no pipeline or history, which carry nothing. Its listing
  goes to standard output as it comes, unless that is the terminal the
  dashboard has: there it is not written, so that thousands of lines do not
  flood it, and the end says how many there were, and to redirect standard
  output to see them. `--no-ui` writes them to the terminal as they come,
  as before. A listing read slowly, as by a pager, holds the run back,
  rather than piling up in memory.
- **Off a terminal, as before.** Where standard error is not a terminal,
  nothing is shown, or the progress line every few seconds with
  `--progress`.
- **Ctrl-C cancels the transfer,** as it does without the dashboard. No
  other key does, so that none cancels it by mistake; `t` only switches
  what the history shows.
- **It starts with the transfer.** Every login happens while preparing,
  before the dashboard opens, so no prompt ever lands in it.

## Where the numbers come from

**Decided.**

- **Counts** come from the peers' reports, as for the progress line. Files
  a second are counted as each file's content has all arrived at the
  Destination, one by one, since the files sent go up a batch of renames,
  up to 256, at a time.
- **The files being transferred** come from the Destination: once a
  second, while there are any, it reports each file handed to a writer and
  not yet done, with its size and the bytes of it forwarded to the writer;
  once there are none, it says so. The client finds each file's rate from
  the bytes it reported over the last 5 seconds.
- **The pipeline's queues** come from the peers too: each server reports how
  full its queues are, in a new kind of report. The Source has the queues
  before reading and compressing, the Destination those before decompressing
  and writing.
- **Each SSH connection's rate and heartbeat** come from the client's own
  transport, which carries every path and heartbeat channel.
- It is drawn with `ratatui` over `crossterm`.

- **The bar counts content as it arrives** at the Destination, before its
  file is done, so that a few large files move it as many small ones do.
  The bytes to send are those of the files whose content is needed, as the
  Destination compares.

**Proposed.**

- **The queue before each stage:** for reading, the Source's files handed to
  its readers; for compressing and decompressing, the frames waiting for the
  pools; for the network, the Source's data packets waiting for the
  connection; for writing, the Destination's files handed to its writers.
  Each server reports its queues four times a second.
- **The bottleneck** is the first stage whose queue is at least three
  quarters full, on average over the last 3 seconds, while the queue of the
  stage after it is not; if every queue up to the last is full, the last.
  With no queue that full, there is none.
- **The time left** is the bytes left over the rate of the last 5 seconds.
