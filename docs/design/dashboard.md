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

 queues                                 now   room   peak
   Source       read                      3     64     12  ▮▯▯▯
                compress                  2     16      4  ▮▯▯▯
                data                    257    256    258  ▮▮▮▮
                send ×4                  34     32     36  ▮▮▮▮
   client       from Source             256    256    256  ▮▮▮▮
                to Destination ×4         9     32     12  ▮▯▯▯
   Destination  receive                   0    256      2  ▯▯▯▯
                decompress                0     16      1  ▯▯▯▯
                write                     1     32      3  ▯▯▯▯
   back         Destination send 0/32 ↑1 · Source receive 0/256 ↑1
   messages     Source 0 ↑1 · Destination 0 ↑1
   manifest     Source 1.0 MiB/8.0 MiB

 warnings
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
  client to the Destination: what waits in each, its room, the most that
  waited, and cells as full as it is; a connection's paths together, as
  `send ×4`. Then, on a line each, those used of the needs and results
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
  scales to the most it shows. It is the first thing to go when the
  terminal is too short for the rest. The client keeps what came in each
  second from the counts it already has; nothing more comes over the
  connections.

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
