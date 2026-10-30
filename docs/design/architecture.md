# Ferry architecture

[中文](architecture.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md), and what goes on each connection, to the byte, in
the [protocol](protocol.md).

**Contents**

- [Client and servers](#client-and-servers)
- [Two peers, Source and Destination](#two-peers-source-and-destination)
- [The client makes the stream between the servers](#the-client-makes-the-stream-between-the-servers)
- [SSH](#ssh)
- [Transport](#transport)
- [Connections](#connections)
- [Monitors](#monitors)
  - [What the user does](#what-the-user-does)
  - [What the user is told](#what-the-user-is-told)
- [Installing on a remote host](#installing-on-a-remote-host)
- [Static musl builds on Linux](#static-musl-builds-on-linux)
  - [Names are looked up with `getent`](#names-are-looked-up-with-getent)
  - [mimalloc is the allocator everywhere](#mimalloc-is-the-allocator-everywhere)
  - [Building and testing the musl build](#building-and-testing-the-musl-build)

![Every transfer: the ferry client starts two ferry servers, one through each location's transport, the Source peer in one and the Destination peer in the other, and makes the stream between them, which it hands to the two; each server runs on this machine or on a host](../assets/architecture-peers.svg)

## Client and servers

**Decided.**

- **The client** is the `ferry` command you run in your terminal. It parses
  arguments, prepares, starts the two servers of a transfer, makes the
  stream between them, and shows what they report: progress, warnings, and
  the final summary. It is never a peer itself, and touches none of what one
  peer sends the other. It is not a background process: it lives as long as
  the command does.
- **A server** is a `ferry server` process that the client starts through a
  [transport](#transport): on a host over SSH, as rsync starts its remote
  side, there by Ferry's [monitor](#monitors), or on this machine as a child
  process.
  Each runs one peer, over the [stream](#transport) to the other server. It
  is not long-lived, and keeps no state between transfers. The only socket
  it listens on is a private Unix socket, for the length of a transfer,
  where the [stream](#connections) reaches it; where SSH forwards to no
  socket, a port on loopback instead, which the stream reaches with a
  secret. Once its peer is done, it says so, and ends as the client ends
  its [control connection](#transport). It is a worker of its
  [monitor](#monitors), the host's or the client's: it stops as its control
  connection ends before, or on `SIGTERM`, or once its monitor is gone,
  which its lifeline says. Whether the client is there is its monitor's to
  judge, not its own.

A transfer runs only while its client runs, and while its stream does:
should the stream break, or an SSH connection drop, the transfer fails, as
with rsync, and nothing makes it again. When the client exits, however it
went, the servers stop too: on this machine, as their lifelines break; on a
host, as its monitor finds the client gone. Files already renamed into
place are complete, so the next run skips them; a file that was not
finished is sent again from the start.

**Decided.** A panic, in the client or a server, is a bug in Ferry: the
process says so, as `internal error in <role>` with the thread's role, puts
the terminal back, removes its temporary files, and exits at once with 101,
leaving no thread to wait on the one that panicked. A server tells its client
first, on its control connection, and the client shows it, as the dashboard
would otherwise hide what the server wrote itself; the client then exits with
101 too.

## Two peers, Source and Destination

**Decided.** A transfer has two **peers**, one for each side. Both run the same
code, each in a server of its own, and the handshake decides which one is the
**Source** and which one is the **Destination**:

| Command | Source | Destination |
|---|---|---|
| `ferry copy ./a host:b` (push) | server on this machine | server on host |
| `ferry copy host:b ./a` (pull) | server on host | server on this machine |
| `ferry copy ./a ./b` (local copy) | server on this machine | server on this machine |
| `ferry copy hostA:a hostB:b` (remote to remote) | server on hostA | server on hostB |

**Decided.** Every combination runs the same way. For each side, the client
gets a transport for its location, starts a server through it, and makes
the stream between the two, over which the peers talk. Where a server
runs only decides which transport starts it; nothing else differs, so
there is one code path to maintain.
Push and pull still name the direction the data goes, and no more.

**Decided.** The Destination decides what to send. The comparison runs on the
Destination, as in rsync; see [pipeline section 2](pipeline.md#2-compare).

**Not chosen** (2026-10-23).

- **The Destination in shards by directory,** each directory's names written
  by one thread, against contention on its lock. A file written without a
  name touches the directory's names only as it is renamed; one decider
  with executors gets the same parallelism, its state with one owner.
- **The Source as threads each keeping a part,** one building the manifest,
  one taking needs, one packing, one sending, each simple to read. What
  they would share, the entries known, what a stream holds, the hashes of a
  large file's segments, would be locks or messages between them, and the
  Source's half could not be a state machine to run against the
  Destination's, from a seed.

## The client makes the stream between the servers

**Decided.** The client makes the stream between the two servers, and
hands each its end: on this machine, through the server's socket; on a
host, as the input and output of a channel over the host's SSH connection.
Between two hosts, it joins the two channels back to back, so that the
kernel carries the bytes from one `ssh` to the other. The stream carries
the [pipes](#transport) between the two peers; the client touches none of
it. What a server reports, such as
progress, a path a dry run lists, or a warning, comes on its
[control connection](#transport), which the client shows. The two machines
need not reach each other, and no credentials are placed on either of
them.

The cost is that, between two hosts, the data still crosses the client
machine's network link twice, as both SSH connections are the client's;
it no longer goes through the client's process, as it did when the client
received each message and sent it again. Measured on this machine, a copy
of 1 GiB took the client's threads half a second of CPU before, and under
10 ms now.

## SSH

**Decided.**

- **Ferry runs the system's `ssh`**, as rsync and File Peeker do, rather than
  an SSH library. `~/.ssh/config`, `ProxyJump`, host aliases, ssh-agent,
  Kerberos, hardware keys, OTP, and `known_hosts` all work as they do for
  `ssh` itself. A library would have to reimplement each of them, and a host
  that `ssh` reaches but Ferry cannot is the failure this avoids.
- **The client holds the connection and authenticates in your terminal.** A
  password or OTP is typed where you ran `ferry`.
- **One SSH connection to a host, and a drop fails the transfer.**
  Everything Ferry runs on a host goes over one SSH connection: its one
  session, which probes the host, installs Ferry there, and becomes Ferry's
  monitor there; the servers the monitor starts, and their control
  connections; and the stream. A host name may lead to several machines, as a cluster's
  login nodes behind one name do; over one connection, everything reaches
  the same one. Should the connection drop, or the stream break, the
  transfer fails, as with rsync, said as `the SSH connection its server ran
  over dropped`, or why the stream broke. Nothing logs in again, or makes
  the stream again, as decided on 2026-10-25, so as to keep SSH's
  uncertainty out of the transfer, as the redesign draft decided that day;
  it is merged into the design and deleted, its last version in `f8085a6`.
  Files already renamed into place stay, and the next run skips them.

**Proposed.**

- **The client installs `ferry` on a host that lacks the version it needs;**
  see [Installing on a remote host](#installing-on-a-remote-host).

## Transport

![The transport: once a connection is ready, it carries pipes, one for each kind of content, one way, in order; on the stream, the Source writes the manifest and the data, the Destination its answers, and control goes both ways; a program writes on its pipe as it would a file, waiting while the pipe's window can take no more, and the other reads what came, the window given back as it reads; the transport frames, schedules and compresses what is written, the data in chunks on every stream, all else on the first; a pipe that ends before its end, or a connection that breaks, fails the transfer; whether the far end is there is not the transport's to judge, but the monitors', by their heartbeat](../assets/architecture-transport.svg)

**Decided.** Every byte between the client and a server, and between the
two servers, goes through one module, the **transport**. Its principles:

1. **Two kinds of connection, both abstract.** Through the transport, the
   client opens **control connections**, each to one program at the far
   end, for preparing and for following a server; see below. And it makes
   the **stream** between the two servers, which it hands to them. Nothing
   of SSH shows, and the transport knows nothing of what a program is:
   where Ferry is installed, or what a token means, is for the component
   to say.
2. **Once ready, a connection carries pipes, each kind of content its
   own.** A **pipe** is one way, in order, and reliable: a program writes
   what it has, as it would a file, and the other reads what came, cutting
   it into its records itself. What one writes, the other reads, in the
   order written: nothing lost, added, repeated, or corrupted. Or the
   connection breaks, and the transfer fails. Before it is ready, while
   the far program and its settings are not yet known, a connection
   carries its bytes as they are; see [Opening is preparing](#transport).
   Which pipes a connection has is fixed by its kind, as both ends are the
   same build: the stream has control, both ways, the manifest and the
   data, the Source's, and the answers, the Destination's; a control
   connection has control, both ways, and the reports, the server's; the
   link to a host's monitor has control alone. See
   [What the pipes carry](protocol.md#what-the-pipes-carry). The connections that
   carry no pipes are those whose far end is not Ferry, or not yet, or
   anyone's: a host's first session, which runs through the login shell;
   sudo's check; a fixture of `ferry test`; and a server's lifeline, its
   standard error.
3. **Its one goal is speed.** Within that guarantee, the transport moves the
   data as fast as it can: nothing is acknowledged or kept on the way.
   How much goes in a frame, when, among what else, compressed or not, and
   whether the far end can take more, is the transport's, and no program
   sees it.
4. **There are as many streams as the round trip needs.** A host has one
   SSH connection, and each stream is a channel of it, or, between two
   hosts, a channel of each, joined; with both servers on this machine, a
   socket pair. One channel carries at most its window, 2 MiB, each round
   trip, so a transfer has one stream for each 15 ms of the round trip to
   its farthest host, as the link to Ferry's monitor there times it, from
   one to eight: K = min(8, max(1, ⌈RTT ÷ 15 ms⌉)), one on this machine.
   The first carries every pipe; the data's bytes are cut into chunks of
   at most 2 MiB, each whole on any of the streams, numbered, and put back
   in order by the reader's transport. A chunk's number comes off the
   wire, and is not trusted. The streams all go over one TCP connection,
   so they break together, and any one broken fails the transfer.
5. **What it adds, it adds inside.** Each pipe's end, sent once its writer
   closes it, the window given back, and syncs go as frames of the
   transport's own, which the transport at the other end takes out, so
   that no program sees them. A writer may `sync` its pipe, which returns
   once the far program has read all written on it before, as the far
   transport counts what it reads, so it costs a round trip and the far
   program's reading; it waits in its scope as any write does. Nothing is
   added where no one syncs, and nothing in Ferry syncs yet.
6. **It does not read what it carries.** A pipe's bytes are the same to the
   transport, whatever they hold; it cannot tell an error from data. What
   goes first is which pipe it is on.
7. **It fails as the connection breaks.** A pipe that ends before its end
   came, as its connection did, or whose connection fails to read, is
   broken; the program reading it then fails, and the transfer with it.
   Every wait on a pipe, a writer's for window among them, is in a
   [scope](#monitors), and returns once that ends: as the connection is
   taken for broken, its host lost, or the transfer stopped. Its silence
   breaks nothing: a program may wait, and say nothing, for as long as its
   work takes. Nothing makes a connection again: a transfer cut short is
   run again, and skips what was done.
8. **A writer waits only for its pipe's window, and memory is bounded.**
   Each pipe but control has a **window**, in bytes on the wire, which its
   reader gives, and gives back as its program reads, a quarter of it at a
   time: a writer's bytes wait in its pipe's send buffer, and go no faster
   than the window lets them. So what comes always has room, and the thread
   that reads a connection never waits for a program. The windows are the
   data's 64 MiB, the manifest's 2 MiB, and the answers' and the reports'
   1 MiB each; a send buffer holds as much as its window, but the data's,
   16 MiB, few enough to stay in the cache. Control has no window: its
   writer never waits for its reader, and it goes first; more than 16 MiB
   unread on it is the far end's fault, as nothing writes there more than
   a little at a time.
9. **Compression is the transport's.** The client decides, for each
   connection, whether its pipes are compressed, and at what level: at
   zstd level 1 to a host, not on this machine, unless `--compress-level`
   gives a level for every connection, or `--no-compress` none; the
   connection's preparation carries it, as the handshake on a control
   connection, the header of a stream, and what the client says first on
   the link. Each pipe but the data's is one zstd stream: its pump takes
   what waits, at most a frame's worth, compresses it, and flushes it, so
   that bytes gather while the wire is busy, and go at once while it is
   not. The data's chunks are compressed each alone, by 8 threads; see
   [Compression](pipeline.md#compression). The windows are of bytes
   compressed. The programs compress nothing.
10. **Opening is preparing.** Every SSH connection is logged in before the
    server starts, and the stream is made once both servers are told to go
    on. After that, nothing logs in. A connection carries pipes only once
    ready, once both ends are the programs that use it and its settings
    are known: what comes before, a shell's output, the start mark, the
    handshake, a port's secret, what a channel to the monitor is for, goes
    as it is, as the programs that read it are in between, a listener, the
    monitor, `sudo`, and no compressor's state goes over two of them.
    Each end has its own state, and need not switch at once: on a control
    connection, the server is ready once it has written its answer, the
    client once it has read the end mark.
11. **Its health is judged by the monitors only.** Whether the far end of
    a connection is there is judged in one place, by the
    [monitors](#monitors): on the link between the client's monitor and a
    host's, a heartbeat goes each way every 5 seconds, and once nothing at
    all came from the far end for 30 seconds, the host is lost; before that
    link, the host's first session says it is alive, each way, as often.
    Anything that comes from the far end counts as its heartbeat does: on
    the link, on a server's control connection, or on the stream, as the
    server reading it says; so a heartbeat waiting behind the data on a slow
    connection takes nothing for lost. A host lost fails
    the transfer, and its monitor, finding the client gone, stops what it
    started. The stream and the control connections carry no keepalive, and
    no one judges them by their silence. No other part of Ferry keeps a
    timer on a connection.

**Decided.** Of a connection's pipes, the pump of its first stream sends
control first, with the window given back and syncs; then the answers, or
the reports; then, of the data and the manifest, the data while a quarter
of its send buffer is full, and the manifest otherwise. An error goes on
control, so it waits behind no data; what must keep its order goes on one
pipe; see [What the pipes carry](protocol.md#what-the-pipes-carry).

**Decided.** Pipes that block both ways could wait on each other for ever,
as two processes that each write to the other, both buffers full. Three
rules keep them from it:

1. **The transport's reader never waits for a program.** It puts what
   comes in its pipe's receive buffer, which always has room, as no writer
   goes past the window.
2. **Window goes back as a program reads, not as it is done with what it
   read.** A program that is slow reads less, so its window is not given
   back, and its writer waits. The data so holds the Source back end to
   end: the Destination's reader takes what it may hold, and, that held,
   reads no more.
3. **No waits form a ring.** A writer held up waits only for a reader that
   never waits for its own downstream: the scan for the manifest's window,
   so for the comparison; the data sender for the data's, so for the
   checkers; both hand on to the decider, which waits for the answers'
   window, so for the Source's thread that reads them, which never waits,
   handing them to the owner on a queue with no bound. On a control
   connection, the reports' reader is the client, which waits for no
   server. So the owner keeps the scan waiting while too many needed files
   wait to be sent, rather than reading no more answers: were it to stop
   reading them, the decider would wait to write its answers, the checkers
   to hand it files, the data pipe for a reader, and the Source for the
   data's window, and the files waiting to be sent would never go down.

A writer held up can still be stopped: its wait is in a scope, as every
wait on a connection is.
Each side sends only its first failure, once, and does nothing more after
it, however its other stages then fail; see
[pipeline.md](pipeline.md#principles). The transport reads none of what it
carries to decide what goes first. A session of the servers' own, which
numbered and acknowledged what went over several streams, and sent again
what was on one that broke, was built in A2 and taken out on 2026-10-25,
with the reconnecting it was for; see the redesign draft's last version, in
`f8085a6`.

**Decided.** A **control connection** is for preparing, for the server's
life, and, while the transfer goes, for following it. It is apart from the
stream.

- **A byte stream to one program.** A control connection joins the
  component to one program on the host, its standard input and output, as
  bytes, much like a pipe. The program at the far end may be one that knows
  nothing of Ferry, such as `sh` before Ferry is installed, so the
  connection carries bytes, not messages; where messages are wanted, as for
  the handshake, the component frames them on the bytes itself. Reading
  comes to an end when the program ends; if it ends in failure, with a
  status other than 0, reading fails, rather than end as if all went well.
  To a server, or a host's monitor on the link, it is ready once the
  handshake, or what the link is for, is said, and carries pipes from
  then on, as the stream does.
- **What it is for.** While preparing, the client opens control connections
  in turn, each to the program it needs: on a host, its first session, a
  script that probes the host, installs this build there if it must, and
  becomes Ferry's monitor there; then, through the monitor, sudo's checks,
  the fixtures of `ferry test`, and last the server, over whose control
  connection the handshake goes, the builds compared, the roles, the
  options, and the token by which the stream joins it is learned, and what
  its root is: its device and inode, and those of each directory above it.
  Both servers then wait, having changed nothing, while the client checks
  their roots against each other: one that holds the other is refused, and
  a Destination that `sync` would empty by mistake asked about; see the
  [overview](overview.md#behaviour-of-ferry-sync). Only then does the client
  tell each to go on, with what it must never go into or delete, and a
  server whose control connection closes instead ends. All of this comes
  before the stream is made. While the transfer goes, the server's control
  connection is how the client follows it: the server sends there what its
  peer reports, its
  [metrics](profiling.md#following-a-transfer-as-it-goes) every second, and,
  once its peer is done, where its threads' time went; and it answers there
  when asked where its threads are; it says there, every second, what the
  stream carried, for the dashboard, and that its peer is done. Apart from
  the stream, none of it waits behind the transfer's data.
- **One session a host, and channels for the rest.** Each session of a
  host's SSH connection runs the user's login shell and its startup files,
  which may take seconds: 1 to 2 on nano4 when it was quiet, 10 to 16 when
  it was loaded, against 40 ms for a channel to a socket. Its output is
  shared with the shell, and with whatever the startup files left running,
  which may write there at any time. So a host has one session, its **first
  session**: a script that probes the host, installs this build there if it
  must, and then becomes **Ferry's monitor** there, `ferry monitor`, which
  listens on a socket of its own, as a server does, and says where in the
  session's output. Everything else on the host goes over channels to it,
  which run no shell. The client follows the monitor on the first, the
  **link** between the client's monitor and this one: once the monitor
  knows what that channel is for, both sides go on it in its one pipe,
  control, with a heartbeat each way every 5 seconds; should nothing at all
  come from the far end for 30 seconds, on the link or otherwise, as when
  the SSH connection dropped without a word, each takes the other for gone, rather than waiting for `sshd` to find
  that, which with its defaults takes over two hours. Before, the first
  session says it is alive, each way, as often, and once it has become the
  monitor, the client must follow it within 30 seconds. A connection to it on which
  nothing comes first, neither its first byte nor its first packet, is let
  go after 30 seconds, so that it holds up none after it, as the monitor
  takes them in turn. On each other, the client has it start one program, a
  worker, which then has that channel as its standard input and output:
  sudo's checks, for `--remote-sudo`, a fixture of `ferry test`, or a
  server, through `sudo` with `--remote-sudo`. The monitor watches each,
  and says on the link how each ended, when one was stopped or went on, and
  what one run to its end, as `sudo`, said on standard error, for the
  client to show; once it has started one, it reads nothing more on its
  channel. One session saves most where sessions cost most: a 1 KB copy
  took 1.2 seconds to nano4 against 2.0 with two, measured in turn on
  2026-10-26. Where one costs 20 to 40 ms, as on fatcat, it took about
  0.02 seconds longer, 0.20 against 0.18, as the first session ends only
  once the link has closed; that was accepted. See [Monitors](#monitors).
- **A server's control connection is the channel it was started on.** No
  shell can write to it, so the server shakes hands there, and its control
  connection stays there until it ends. On this machine, the client starts
  the server itself, through pipes.
- **The server ends as the client ends its control connection.** Once the
  handshake is done, the control connection carries two pipes: control,
  both ways, on which the client tells the server to go on, or asks where
  its threads are, and the server says its metrics and what the stream
  carried every second, its failure, and that its peer is done; and the
  reports, the server's, of what its peer reports, and last, where its
  threads' time went. Once its peer is done, the server says so, and waits
  for the client to end its control connection; the client's monitor ends
  both once both servers are done, and each server then ends its pipes,
  having written all it said, so that the client hears it out once both
  pipes have ended. Ended before, the server stops. How long one server is waited for, once the other is
  done, or failed, is no time of its own: what can be seen is waited for;
  see [Monitors](#monitors).
- **Its silence says nothing.** A server that says nothing, as its peer
  works, or waits on a slow disk, is not taken for gone: its process is
  watched, on this machine by the client's monitor, on a host by the
  host's, which tells the client how it ended, and when it was stopped or
  went on. Every second, the server says on it what the stream carried, for
  the dashboard.
- **The server is bound to its host's SSH connection.** Its control
  connection and the stream go over it, so losing it ends the server, and
  the transfer.
- **Its pipes are compressed as the stream's are.** Most of the time it
  carries little, a few hundred bytes a second; but a dry run's paths, or
  many warnings, are a great deal, and compress well. To fatcat, measured
  against the build before the pipes, whose control connection went as it
  was: a dry run listing 42,359 paths took 1.9 MB there, against 3.7 MB,
  and 10,000 files that could not be read 0.3 MB, against 0.9.
- **A dry run's paths go a report each.** Each costs the Destination about
  a microsecond more than when they went in batches, most of it on its
  decider, before the pipe: a dry run of a million empty files took 1.66 s
  on this machine, against 0.83 s before the pipes, and 2.5 to 3.5 s on
  nano4, against 1.0 s.
- **A peer's failure is told on it too.** The server says it as its peer
  fails, and the client takes the first either server says as the
  transfer's cause; the other peer learns it from the stream's control
  pipe, which goes ahead of all else.
- **Where it runs is the transport's.** Like everything below, it is
  hidden. It shares the host's SSH connection with the stream, so it needs
  no login of its own.
- **The stream reaches a socket.** The stream, as each channel to the
  monitor does, reaches the server's socket: straight, as a channel SSH
  forwards to it; where SSH forwards to no socket, at a port on loopback
  instead, giving a secret first; with `--remote-join`, by running `ferry
  server --join <token>` there; or, on this machine, handed over through
  it; see [Connections](#connections). Only the stream comes to a server's
  socket, and what it reads first says so, which stream it is, and the
  level its pipes are compressed at; on the monitor's, what comes first
  says what the connection is for.
- **What Ferry says in a session goes in marked lines, and the server's
  answer between marks.** On a host, the first session runs through the
  user's login shell, whose startup files may print to standard output, as
  a tool that warns there does, and may leave a program running that
  prints later. So each line the session's script, and then the monitor,
  says there is `\0ferry:`, a tag, the line's kind, and the length of its
  content, each followed by `:`, then the content and a newline, in one
  write, which a pipe keeps whole among others' writes. The tag is the
  client's own, new each run, so that nothing printed by chance is taken
  for a line, and the length finds anything printed into one; each part,
  such as the probe's, ends with a line that counts those before it, so
  that one lost shows. The client reads only those lines, and says once
  what else the shell printed, for its startup files to be fixed. The
  server sends one message on its standard output, its answer to the
  handshake, and writes it between a start mark, `\0ferry: start\0`, and
  an end mark, `\0ferry: end\0`, in one write of a few hundred bytes: the
  monitor's channel carries nothing else, but one that joins through the
  shell may carry what the shell left running prints. The client skips
  what comes before the start mark rather than reading it as the answer; an
  answer without its end mark was cut into, and fails the handshake. A
  program whose output ends before the start mark did not start. What
  listens on a socket, the monitor or a server, answers a channel that
  reaches it straight, or by its port, with the start mark, and a `--join`
  process writes it as it starts, after what the shell printed; where the
  other server is on this machine, the client writes it, as that one
  writes nothing before its pipes. So the stream begins with one
  start mark each way, which the server reading it skips to.


**Decided.** There is more than one transport, and the client does not know
which one it has. For each location, it asks for the transport of that
location, and uses only the two kinds of connection.

- **The SSH transport** reaches a host, as this section and
  [Connections](#connections) describe.
- **The local transport** reaches this machine. A control connection starts
  its program as a child process, joined by pipes. Its stream is a socket
  pair, whose ends the client hands to the servers through their sockets,
  holding each until its server says it took it: on macOS, a socket in
  flight to a connection not accepted yet, which no process holds, is
  emptied as garbage.
  There is nothing to log in to, probe, or install: the server is the
  client's own binary.
- **Another way to reach a host** is another transport. Nothing above the
  transport changes for it.

**Not chosen** (2026-10-28, with the pipes).

- **The transport cutting large messages into packets,** each directory
  one message. The reader would need all of one to decode it, its memory
  going up at once, and the Destination would wait for a directory whole
  before comparing it: worse than the parts the peers cut.
- **Keeping the parts, and having the stat threads take names from a whole
  directory.** It mends only how the stats are shared out; a message's
  size, the needs, the Source's known entries and the flow control stay
  tied to the parts.
- **A message for each entry, not packed together.** Each would pay a
  packet's and a hand-over's cost; packing is the transport's to do.
- **The data compressed by the peers, the manifest alone by the
  transport,** the pipes draft's first version. A compressed stream in the
  transport would be one thread, holding the data back; but that only says
  the transport's compressing must be parallel. Blocks compressed alone
  were for sending one again, gone with the reconnecting, and for putting
  them back in order, which a pipe does.
- **The handshake inside the control pipe, the build compared by an
  argument to the server.** It would let the handshake be compressed, which
  only makes it larger; and the handshake carries the pipes' own settings,
  which must come before them.
- **An SSH channel for each kind of content, with OpenSSH's windows.** A
  socket pair on this machine has none; a channel's window is a fixed
  2 MiB, too little at a long round trip; and between two hosts a stream
  is two channels joined, whose windows are not end to end.
- **Gathering a pipe's bytes for up to a time, as 50 ms, before
  compressing.** On a fast connection it costs a wait after each flush,
  and nothing is gained; on a slow one it compresses better; see
  [Compression](pipeline.md#compression).
- **gRPC, HTTP/2, QUIC.** An RPC framework brings framing of its own, and an
  async runtime, for messages Ferry frames itself, on threads that block.
  QUIC would have each host open a UDP port, where the client makes every
  connection; it too needs an async runtime, UDP is blocked more often than
  TCP, and a connection's packets are handled in user space, at a cost in
  CPU.

**Decided** (2026-10-26). A channel of an SSH connection can have only so
much data unacknowledged, its window, 2 MiB in OpenSSH
(`CHAN_SES_WINDOW_DEFAULT`), so it carries at most its window per round
trip: about 17 MB/s at 120 ms. A TCP connection, for its part, is held back
by congestion control and losses, whatever its channels. On a path with a
long round trip, the data's chunks so go over several channels of the
one SSH connection, which need no login, so that the window is not the
limit; on a short one, as to fatcat (1.7 ms) or nano4 (6.5 ms), there is
one, as more were no faster there. The heartbeat waits behind all of them
in the one TCP connection, which the data coming meanwhile covers; see
[Monitors](#monitors).

## Connections

![Connections to a host: the ferry client runs one ssh master for the host, your own ControlMaster or Ferry's, with one TCP connection; over it run the host's one session, through the login shell, which probes the host, installs Ferry, and becomes Ferry's monitor there; channels sshd forwards to the monitor's socket, with no shell, one that follows it, the link between the two monitors, and on each other a server the monitor starts, whose control connection it is; and the transfer's streams, as many as its round trip needs, each a channel sshd forwards to the server's socket, whose other end the client hands to the other server; should it drop, the transfer fails](../assets/architecture-connections.svg)

**Decided.** This is how the [transport](#transport) uses SSH. One `ssh`
master per host, and one TCP connection. Over it run the host's one
session, which becomes Ferry's monitor there; the channels to the monitor,
each server's [control connection](#transport) among them; and the stream,
which joins the servers. The SSH connection is logged in before anything
runs on the host.

- **One SSH connection to a host, sharing your own ControlMaster if you
  have one.** The client reads the options that apply to the host from
  `ssh -G <host>`. If they set up a ControlMaster (`controlmaster` is not
  `false` and `controlpath` is not `none`), it adds no control options, and
  the SSH connection is your master's. Otherwise it uses one of its own:
  `ControlMaster=auto`, `ControlPath=/tmp/ferry-<uid>/ssh/%C`, and
  `ControlPersist=600`, with `ServerAliveInterval=15` and
  `ServerAliveCountMax=4`, so that it ends, and everything on it, once the
  host has not answered for about a minute: nothing else watches what is
  done before the transfer, as an upload, which would otherwise wait on a
  connection that is gone until TCP gives up, for hours. Your own master
  keeps the options you gave it. Either way a login or OTP is typed once
  per host, and a later run within the persist time needs none. The control
  sockets live under `/tmp` because a Unix socket path is limited to 103
  bytes on macOS. No setting turns this off: a ControlMaster of your own is
  found without one. Every `ssh` Ferry runs also gets `RemoteCommand=none`,
  `ClearAllForwardings=yes`, `PermitLocalCommand=no`, and `ForwardX11=no`,
  as its sessions run their own commands and need no forwarding, and
  `ConnectTimeout=10` unless `ssh -G` shows one of yours.
- **Everything on a host goes over its one SSH connection.** A host name may
  lead to several machines, as a cluster's login nodes behind one name do,
  and each new SSH connection may land on another: a stream over a second
  connection could reach a machine where the server is not. One master,
  `%C` naming the host, user, and port, holds the one TCP connection, so
  everything Ferry runs reaches the same machine. `--ssh-connections`, which
  opened several, was removed on 2026-10-25; a command line that still has
  it is refused, with exit status 2.
- **The SSH connection is opened first, then the server is started.** As
  the [overview](overview.md#reliability-comes-first-then-speed) requires,
  the transport first opens the SSH connection: it starts the master and
  waits until it is logged in. Only then does the client, in the host's
  one session, probe the host, install Ferry if needed, and start Ferry's
  monitor, which starts the server, so nothing after the login asks for
  a login.
- **The SSH connection is opened with no task.** The transport runs
  `ssh -N`, which logs in and starts the master but runs nothing on the
  host. Opening it is only logging in; the first session, the channels,
  and the stream are opened later over the master already logged in, with
  no login of their own. With `ControlPersist`, as Ferry's own master has,
  `ssh -N` then leaves the master running in the background, and ends.
  Without it, as your own ControlMaster may be, `ssh -N` is the master, and
  the transport holds it for the whole run, as the master would otherwise
  end with the first `ssh` that used it; it ends with Ferry, however Ferry
  ends, `SIGTERM` and `SIGHUP` too, which end it before Ferry exits; only
  `SIGKILL` leaves it, as nothing then runs. A master already up, your own
  or one Ferry left running within its persist time, is used as it is, and
  no `ssh -N` runs: given a master, `ssh -N` asks it for a session, which
  starts a login shell on the host.
- **It logs in quietly first, then in the terminal.** The client first tries
  `BatchMode=yes`, which succeeds with key authentication or a master
  still alive; if that fails, it lets `ssh` ask in the terminal. Between
  two hosts, one logs in after the other, so that prompts never interleave
  and the progress line never cuts into them.
- **If the SSH connection cannot be opened, preparing fails.** A failed
  login, one that needs a terminal when there is none, or Ctrl-C during a
  login stops the run before the transfer starts, with nothing to clean up,
  and says why, such as `ferry: fatcat: the SSH connection needs a login,
  and there is no terminal to type it in`.
- **Its streams.** A transfer has K streams, as many as the round trip
  needs, which the client makes once both servers are told to go on, and
  how many: each a channel over the host's SSH connection to a server on a
  host, or a socket pair to one on this machine. Each says its number
  first, in its header, with the level its pipes are compressed at, and
  the server takes them all before its peer starts.
- **The stream reaches its servers with no shell.** Once a server is
  running, it waits on a Unix socket in a private directory on the host,
  named by a random token, both learned over its control connection. On a
  host, the stream's end is a channel that SSH forwards to that socket, as
  `ssh -W <socket>` opens: no shell runs for it, so nothing a shell or what
  it leaves running prints can come in between. A server opens no port, but
  where SSH forwards to no socket; see below. The client makes the stream as
  a socket pair: one end is the channel's input and output, the other is
  handed to the server on this machine, or is the input and output of the
  other host's channel, so that the two `ssh` processes are joined back to
  back.
- **The client writes what each server reads first.** Into each end, before
  it hands either over: for a channel that comes straight, the byte that
  says so, or, for one to a port, its secret; the header, which says which
  stream it is, and the level its pipes are compressed at, or none; and,
  where the other server is on this machine, the start mark. A server on a host answers a channel that comes straight, or by its
  port, with the start mark; so the stream begins with one start mark each
  way, which the server reading it skips to.
- **Where SSH will not forward to a socket, a port on loopback; where it
  forwards to nothing, `--remote-join`.** A host's `sshd` may forbid
  forwarding to a socket, with `AllowStreamLocalForwarding no`, and one
  before OpenSSH 6.7 cannot do it, though either may forward to a port. So
  the monitor listens on a port on loopback too, and says it, with a
  secret of 64 hexadecimal digits, beside its socket; the client tries the
  socket first, then the port, and the first channel that gets through
  decides: the monitor closes the other, and the servers it starts listen
  only on the one that got through. A port is anyone's on the host to
  connect to, so each connection to one gives the secret first, which the
  client learned over the first session, or the server's control
  connection; one that gives another, or nothing within 30 seconds, is let
  go of, and at most 16 are waited on at once. Where neither gets through, preparing
  fails, naming `--remote-join`. With it, each channel to the monitor,
  and the stream's, run `ferry server --join <token>` on the host instead,
  which writes the start mark and hands its standard input and output to
  the monitor, or the server, through its socket. Such a channel runs
  through the user's shell, and only what the shell prints before the
  start mark is kept out: what it leaves running may still write into it.
  So it is not done unasked, and it costs a session each. A joining
  process stays until what it joined is done with what it handed over, as
  sshd closes a session's input once its process exits; so the stream ends
  with its pipes' ends, not as the channel closes, and the monitor stays as
  long as it is followed.
- **The stream's readers judge only its pipes' ends.** A stream that ends
  before each pipe on it has ended is broken: the reader shuts it down both
  ways, where it is a socket, so that its other end sees it broken too, and
  the peer reading it fails, and the transfer with it. Nothing on the stream says
  the other side is alive, and its silence breaks nothing: whether a host
  is there is for the monitors to say, by the heartbeat, and by what the
  stream carried, as its servers tell them. The heartbeat's 30 seconds,
  and the first session's, are the only timers on a connection, but for the
  client's own while it logs in, 30 seconds for a master that does not
  answer, and the 30 seconds the monitor, and a server, give what comes
  first on a connection to its socket or port.
- **What the SSH connection carries is told for the dashboard.** Each
  server says every second what the stream carried, both ways; the client
  shows it for the host's SSH connection, as the
  [dashboard](dashboard.md#what-it-shows)'s rate, and how long since
  anything came from the host, its heartbeat at least every 5 seconds, as
  "heard".

**Decided.** Every connection a transfer uses, in the order they are made,
how each is made, and what goes on it, to the byte, is in the
[protocol](protocol.md#every-connection).

**Not chosen.**

- **Several SSH connections to a host** (`--ssh-connections`, built in A2
  and removed on 2026-10-25). On a lossy path a single TCP flow is held
  well below the link's capacity by congestion control: to one host with
  about 0.01% loss, one connection reached about 31 MB/s, and four together
  about 60 MB/s, the link's limit. But each connection may land on another
  machine behind the host's name, each costs a login, an OTP each where the
  host asks for one, and spreading one transfer over them needed a session
  of the servers' own; see the redesign draft's last version, in `f8085a6`.
- **Making a broken stream again** (built in A2 and removed on
  2026-10-25). The client made another stream over the same SSH connection,
  logging in again first if it had dropped, and the session sent again
  what was on the broken one, so a transfer went on through a brief drop.
  It cost a session that numbered, acknowledged, and kept all it sent until
  the other side had it, a grace of a minute for the servers to wait, and
  logins in the middle of a transfer. A transfer that fails is run again
  instead, and skips what was done, as with rsync.
- **More sessions, or forwarded ports, over one master, for more flows.**
  SSH carries every session and every forwarded port of a master as
  channels over its one TCP connection, so they share one flow and its
  congestion control. Four sessions over one master were no faster than
  one.
- **A TCP port of Ferry's own.** Connecting straight to a port the server
  opens would give several flows without more logins. But it needs
  encryption of Ferry's own, with a key passed over SSH, a port open on the
  host, and a fallback to SSH where a firewall blocks it; and the two
  machines would have to reach each other, which the client making the
  stream never needs. The port on loopback above is not one: it is reached
  only through SSH, as a channel of the host's SSH connection.
- **Probing and starting the server in one session.** Where the build is
  there, the probe would become the server. It saves a session in the
  usual case, but a server would start before another host is asked
  about, or has, its install; sudo's checks do not fit; and installing, or
  both sides on one host, save nothing.
- **A loop of `sh` in the first session,** reading a request a line,
  running it, and answering in marked lines, which needs no Ferry on the
  host. But it does one thing at a time, so a second server on the same
  host, or a fixture, needs a session again; and an upload after a request
  must be read to its exact length, which only GNU's `head -c` does, and
  `dd bs=1` at a system call a byte. Nor can it be a script read from
  standard input (`sh -s`): dash reads ahead, and would run what follows
  as commands.
- **Installing by upload alone.** It needs nothing of the host, and a
  download saves only about 5 MB; but a download, as VS Code's, is the
  default since 2026-10-25, the upload left for where it fails.
- **Keeping the monitor between runs,** as VS Code keeps its server, which
  would save even the first session. A name may lead to another login node
  next time; a process left on a login node may be cleaned away; it would
  keep the environment it started with after `.bashrc` or a module
  changed; each build would need its own; and when to end it would need a
  rule.
- **A line in `.bashrc` that returns early for Ferry,** matched on a word
  in Ferry's command, which would save the one session's startup files,
  1.6 to 3.7 seconds on nano4, for those who add it. Declined on
  2026-10-25.
- **Sending `SHLVL`,** with which bash reads no `.bashrc`: the host's
  `sshd` takes only the variables its `AcceptEnv` names, by default `LANG`
  and `LC_*`.
- **Remembering the last probe.** Each login node behind a name has its
  own machine ID, and so its own build's path, and the roots differ from
  run to run.
- **An output of the first session's own, by `ssh -R` back to the client.**
  Only bash's `/dev/tcp` connects out, not `sh` or dash; the host must
  allow remote forwarding; and it opens a port on the host's loopback that
  any user there can reach. Marked lines are enough.
- **Hosts connecting to each other straight, as mosh does,** keys handed
  over SSH. A host's data would not go through the client, the client could
  leave, each stream would be encrypted on a core of its own, and a
  connection would be made again with the key, no login. But a server would
  listen, or reach out, on a port, the two hosts would have to reach each
  other, and a layer of encryption, Noise or TLS, would be Ferry's to keep,
  where the client makes every connection; the window and the logins it
  would get around, the channels of one SSH connection already do
  (2026-10-23).
- **One host running `ssh` to the other, with the agent forwarded.** It
  puts credentials, or a forwarded agent, on a host, where Ferry puts none;
  and an OTP cannot be forwarded.
- **The peers in the client's process where a side is on this machine.**
  Two processes fewer, no socket and nothing serialized. But the client
  would be a peer, which it never is; server control, removing temporary
  files at a stop, and the stages would need a second kind in the client;
  and local tests would no longer go the way a host's do. The client hands
  the stream's two ends to servers on this machine as on a host, and local
  to local differs in options alone (2026-10-23).

## Monitors

**Decided** (2026-10-26).
Whether something is alive, when it should end, how what a lost connection
leaves is cleaned up, how the user's Ctrl-C reaches every side, and who says
what is stuck, is the **monitors'**: one in each place, the client's machine
and each host, the top of what Ferry runs there. The peers know none of it.

- **One in each place.** The client's monitor is a thread of the client: it
  holds the transfer, each host, and every process the client starts, the
  `ssh` processes and the servers on this machine among them. A host's
  monitor is `ferry monitor`, what the host's first session becomes; it
  holds what it starts there.
- **One core, which does no I/O.** Both run the same state machine, told
  what happens one event at a time, with the time, and saying what to do:
  end a scope, signal a process, close a connection, tell a host's monitor
  to stop, tell the user. So what they do is tested on events and times
  made up. Threads around it do the I/O: the heartbeat, a watcher for each
  process it started, by `waitpid`, which says a process was stopped or
  went on as well as how it ended, the signals, and each connection.
- **Every wait is in a scope.** A monitor holds a tree of scopes: on the
  client, the transfer, each host under it, and under each host its link,
  its servers, and what carries their connections; a server on this
  machine hangs from the transfer itself. Each wait, on a connection, a
  listener, or a process, is in one, and returns once what it waits for
  comes, or its scope ends, as a host's does once it is lost: so nothing
  waits on a host after it is gone, and no wait keeps a time of its own.
- **A connection is a wire.** Only the transport holds a connection's
  descriptor. All else reads and writes it as a **wire**, whose waits are
  in its scope: the raw bytes first, as what a shell printed, the start
  mark, the handshake, a port's secret; then, once it is ready, its pipes'
  frames. Its ends do not block; a host's monitor that hands one to a
  program it starts makes it block again first, and keeps nothing it could
  read or write. So that every wait on a connection returns once its scope
  ends holds by type, as it does for a wait for a connection to come, on a
  listener, and for a process, by its watcher. Two writes are not on a
  wire: the lines a host's monitor writes on its first session's output,
  saying where its socket is, which block, as that output is shared with
  what the user's shell left running, whose writes would fail if it did
  not; and `ferry build-id`, which prints its few bytes as any program
  does, on a channel only for `--remote-sudo`'s check. Each is written
  once, a few hundred bytes at most, and what started it cleans up after a
  lost connection.
- **One silence.** Three times end anything, each the design's one
  silence, 30 seconds: how long a host may be silent before it is lost; how
  long what is awaited before a heartbeat may take, as a monitor's being
  followed, or a connection's saying what it is; and the grace a host that
  lost its connection gives what it started. Other times only change what
  is shown, or how a key is read: a wait is said past a second, a second
  Ctrl-C forces within 3 seconds, and where every thread is, is said once
  nothing moved for 60 seconds. Logging in has `ssh`'s own limits, and 30
  seconds for a ControlMaster that does not answer. A wait on a third party
  on the network has a floor on its progress: a download with `curl` gives
  up after 10 seconds without connecting, or 30 below 1 KiB/s; with `wget`,
  after 30 seconds of silence. A wait on a person is not timed; see below.
- **A host's Liveness.** On the client, whether a host is there is said by
  one source at a time: while it logs in, `ssh`'s own limits; then the
  first session's lines, an `alive` among them every 5 seconds; then the
  link's heartbeat, and anything else that comes from the host: anything
  on a server's control connection, or what a server's stream carried
  from there, as that server says every second. Silent for 30 seconds,
  the host is lost: the transfer fails, saying so, as `nothing came from
  fatcat for 30 s`, and nothing more of the host is waited for. So it is
  while the transfer ends for a failure elsewhere too, as what is waited
  for on the host comes over its connection, until its link closes in
  order, once all there is done. The first
  session's script likewise hears the client every 5 seconds, and ends the
  whole session once nothing came for 30; the host's monitor, the link's
  heartbeat, and what its servers' streams carried from the client's side,
  which each tells it every second, in a marked line on its lifeline. The
  heartbeat shares the SSH connection's one TCP connection with the data,
  and waits behind it: by seconds where the connection is slow, or has
  lost packets, as measured on 2026-10-26, which the data coming
  meanwhile covers. So does a stop, or what a host reports: on a slow
  connection, it may come seconds late.
- **The servers are workers.** A server has no heartbeat, and no tree of
  scopes: its waits are in one scope that nothing ends, as the server ends
  whole: as it is told to stop, its control connection closing, or by
  `SIGTERM`; as its monitor is gone; or as it is done. Its monitor
  gives it its standard error as its **lifeline**: one end of a socket
  pair, whose other end the monitor holds, reading what the server says
  there. The server waits for it to be readable, which it is only once the
  monitor is gone, however it went, `kill -9` too, and then stops, as on
  `SIGTERM`. Standard error is what `sudo` keeps open, so the lifeline
  holds through `--remote-sudo`; where `sudo` puts a pipe of its own in
  between, as its I/O logging does, the server sees it, and says its
  lifeline cannot be relied on.
- **What can be seen is waited for.** A server that is stopped, as by
  Ctrl-Z or `SIGSTOP`, is said to be, and waited for: once it goes on, so
  does the transfer. Whatever is awaited past a second is said, with how it
  looks from outside. Only a host whose connection is lost, which no one
  can see any more, gives what it started 30 seconds after `SIGTERM`, kills
  what is left, and ends; and the client waits for nothing more of it.
- **Process groups of their own.** Every `ssh` that goes through the
  master, and each server on this machine, runs in a process group of its
  own, so that a Ctrl-C in the terminal reaches the client alone, which
  decides how each ends. The `ssh` that logs in stays in the terminal's, to
  read a password or OTP there.

**Not chosen.**

- **A keepalive on each connection, and a time limit at each stage none
  could carry,** as before 2026-10-26. Each connection stood on its own;
  but the limits were spread over a dozen places, and each new connection
  or stage needed one remembered: some were missed, and waited without end
  on a connection dropped without a word.
- **A monitor in each process.** Each server had its own tree of scopes
  and a heartbeat. One in each place, the servers its workers, is a level
  less.
- **What a host's monitor starts not depending on it,** the launcher's rule
  until 2026-10-26. What a lost connection left running on the host was
  left there; a monitor that stops what it started cleans it up.
- **A host that waits for the client as little as it can, before its
  monitor.** The client would give the first script all the host needs to
  decide, and the host would download, install and become its monitor by
  itself, waiting only for an upload. It saves a few round trips, about
  40 ms each to nano4, against a session's 1 to 2 seconds there; an upload
  still needs the heartbeat both ways; and without `--yes` it changes when
  the user is asked. A faster start can be had apart from health.
- **A lifeline other than standard error.** Another descriptor, as 3, is
  closed by `sudo`'s `closefrom`, whose `-C` works only where sudoers sets
  `closefrom_override`. Linux's `PR_SET_PDEATHSIG` is cleared as `sudo`, a
  setuid program, starts, and macOS has none. A pipe cannot be waited on
  quietly on macOS, which says its reader is gone only to one waiting to
  write, and wakes that one all the time.
- **The client catching Ctrl-Z,** to tell each host's monitor to stop
  judging its silence before it stops. Stopped so, a connection that
  dropped without a word would leave what it started on the host with no
  one to end it. So a client stopped past 30 seconds fails the transfer,
  unlike rsync, which has no timeout by default.

### What the user does

| What the user does | What happens |
|---|---|
| Ctrl-C, `SIGTERM`, or the terminal closed (`SIGHUP`) | The client tells each host's monitor, on the link, to stop, which sends what it started `SIGTERM`, and says how each ended; each server on this machine gets `SIGTERM`. The client says `stopping; press Ctrl-C again within 3 s to stop at once`, and, past a second, what it still waits for. Once all have ended, it exits as the signal says, 130 for Ctrl-C. |
| A second Ctrl-C within 3 seconds | Forced: each process the client started is killed, each link closed, and the client exits at once; each host's monitor, its link ended, stops what it started within its grace. |
| A second Ctrl-C after 3 seconds | Counts as a first: the client says again what it waits for, and gives another 3 seconds, so that two presses by mistake do not force. |
| The client killed, as by `kill -9` | Nothing of the client runs: the servers on this machine find their lifelines broken; each host's monitor finds its link ended, or silent, stops what it started, and ends. |
| Ctrl-Z | Stops the client alone: the hosts stop hearing its heartbeat, and after 30 seconds take it for gone. |

A question to the user, whether to install on a host or to sync into a home
directory, waits as long as it takes, but ends at the first Ctrl-C, `SIGTERM`
or `SIGHUP`.

### What the user is told

A monitor sees how what it started looks from outside, from the system,
though the program itself can say nothing, as when all its threads wait on a
filesystem that does not answer:

- **On Linux,** from `/proc`: each thread running, waiting, stopped, or in a
  wait nothing interrupts (D state), not even `SIGKILL`; where in the
  kernel it waits; and, in such a wait, its system call and its file, as
  `reader is in a wait nothing interrupts, in the kernel at
  request_wait_answer, which waits for a FUSE filesystem, in pread64() on
  /mnt/x`. NFS, FUSE, SMB, Lustre and Ceph are named where the kernel
  function's name says.
- **On macOS,** with `proc_pidinfo`: whether the process is stopped, and
  each thread running or waiting. A wait nothing interrupts could not be
  made there to try.
- **Elsewhere,** only that it is still there. A server through `sudo` runs
  as root, and is seen only as there, or gone.

Each of Ferry's threads is named by its role, so that the system shows which
one waits. A host's monitor says it on the link when the client asks; the
client reads its own. It is said whenever the client says what it waits
for, as a server is stopped or goes on, at `Ctrl-\`, and once nothing has
moved for 60 seconds, beside what each server says of its own threads, as
before; see [profiling](profiling.md#a-snapshot-of-every-thread). A look
decides nothing, and times nothing.

## Installing on a remote host

**Decided.** A server on this machine is the client's own binary, so there is
nothing to probe or install for it. On a host:

- **A client and a server run exactly the same build.** The protocol needs no
  compatibility with older versions, and can change freely between them. The
  handshake still compares builds, and refuses a mismatch with a clear error
  instead of running the wrong one silently. The build is read before the
  rest of the hello, so this is the one layout every build keeps: the hello
  first among control messages, its build first in it, and the refusal's.
- **Builds are installed side by side.** Each build has a directory of its own
  on the host, and the client runs the one matching itself. Two clients of
  different versions, say on a laptop and a desktop, can reach the same host
  without overwriting each other. Since servers are not long-lived, there is
  never a running server to replace.
- **Only the release workflow builds a release build.** It sets
  `FERRY_RELEASE` at compile time. Any other build is a dev build, even one
  optimised with `cargo build --release`, because its source may differ from
  the release of the same version.
- **Installing happens in the host's one session,** after the probe and
  before the session becomes Ferry's monitor, so it costs no session of
  its own. An upload, of a release's binary or a dev build's source, is the
  last thing the session reads: what installs it reads it to its end, and
  then becomes the monitor itself.
- **A release build is downloaded by the host itself,** as VS Code's server
  is: the release of the client's own version for the host's OS and
  architecture, from GitHub Releases, with `curl` or `wget`, checked against
  the SHA-256 that the release's `SHA256SUMS` lists for it, which the client
  fetches, keeps, and gives the host with the address. Where the host has
  neither tool, or neither `sha256sum` nor `shasum`, or its download fails
  or does not match, the client uploads the binary instead, in the same
  session: it downloads it itself with `curl`, checks it against the same
  list, caches it locally, and uploads it over the SSH connection. So the
  host needs no outbound network access, which cluster compute nodes often
  lack. Linux binaries are [static musl
  builds](#static-musl-builds-on-linux), so they run on any distribution's C
  library. The host puts a download or an upload in place, by a rename, only
  once it has checked it: its SHA-256, and an upload's size, which is all
  that is checked where the host has neither `sha256sum` nor `shasum`; then
  the binary must run there, and say, as `ferry build-id`, that it is the
  build it is installed as. So a download or an upload cut short, by Ctrl-C
  or a dropped connection, installs nothing, and a binary that cannot run
  there, as where the home directory is mounted `noexec`, is found at
  install. Each install writes a file of its own, so two clients installing
  at once do not mix their binaries.
- **The host's download has not run on a real host yet.** Only a release
  build takes it, and no release has had it yet: v0.16.0 came before it.
  Its scripts are tested on their own: a download that fails says why, and
  the session goes on to read the next command, as for an upload.
- **A dev build installs from source,** as in File Peeker. A dev build has no
  release to download, so the client uploads the source it was compiled from,
  and the host builds it. The build is identified by a hash of that source.
  The host needs a Rust toolchain. What it built is installed only if it says
  it is that build: source that did not all arrive builds another. One dev
  install at a time builds on a host, since they share the source and the
  target directory; another waits.
- **The client asks before the first install on a host.** Later installs of a
  newer version on the same host do not ask. Without a terminal, the client
  installs only when given `--yes`.
- **Ferry never touches the host's `PATH`.** It runs the build it installed,
  even if another `ferry` is on the `PATH`.
- **Old builds are removed.** A host keeps the three most recently used
  builds. Each use of a build, and each install, marks its directory used, by
  its modification time, and removes the others beyond three, but for any
  used in the last hour: another client may have just probed it, and be
  about to start it. The age comes from `find -mmin`; on a host without it,
  as some container images are, the age is not known, and all but three are
  removed, so that old builds never pile up.
- **Releases are built by CI.** A release workflow, on a tag `v<version>`
  that must be the version in `Cargo.toml`, builds and tests each platform on
  a runner of its own architecture: Linux, static musl, and macOS, on x86_64
  and aarch64. It publishes each binary, uncompressed, as
  `ferry-<version>-<target>`, with a `SHA256SUMS` of them, and an
  `install.sh` with the version written in, which downloads, checks, and
  installs the binary for the machine it runs on. The latest release's is
  what `curl -fsSL <repository>/releases/latest/download/install.sh | sh`
  runs.

**Proposed.**

- **Where builds live.** Builds are installed in
  `~/.ferry/hosts/<machine-id>/bin/<build>/ferry`. A release build's `<build>`
  is its version, and a dev build's is `dev-<source hash>`. The machine ID
  keeps apart the nodes of a cluster that share one home directory but may
  differ in architecture or C library. A dev build uses `~/.ferry-dev/`
  instead, so it never meets a release install, as in File Peeker.
- **How a dev build is uploaded and built.** It uploads the files that
  `git ls-files --cached --others --exclude-standard` lists, so uncommitted
  edits travel too. The host builds with `cargo install --locked` into a kept
  target directory, so that a rebuild is incremental.
- **`--remote-ferry <path>`** runs a given binary instead, like rsync's
  `--rsync-path`. It is for hosts where the home directory is not writable or
  has no room.

## Static musl builds on Linux

**Decided.** Linux releases are static musl builds, so one binary runs on any
distribution's C library, the old glibc of RHEL clones included. musl differs
from glibc in ways that shape the design, gathered here.

### Names are looked up with `getent`

musl reads users and groups only from `/etc/passwd` and `/etc/group`. It does
not support NSS, so it cannot see accounts in LDAP or SSSD, which is where
most clusters keep them. To map owners and groups by name there, the Linux
build looks names and ids up by running the system's `getent`, which uses the
system's own C library and so sees everything NSS sees.

- **Lookups are batched and cached.** `getent passwd` and `getent group` take
  several keys in one call, and each id or name is looked up once per run. A
  side runs `getent` only for a batch of entries that brings ids or names it
  has not seen, so the number of processes grows with the distinct owners and
  groups, not with the files: a tree with millions of files costs a handful
  of processes, not millions.
- **Without `getent`, Ferry falls back** to `/etc/passwd` and `/etc/group`
  through musl, then to numeric ids, and warns once.
- **A name made only of digits is not mapped.** `getent` takes such a key as
  an id, so it finds the account with that id, or none, instead of the one
  with that name. The entry keeps its numeric id, as when the name is unknown.
- **Nor is one that cannot be an account's:** empty, longer than 256 bytes,
  with a `:`, a `/`, a NUL or a space in it, or starting with `-`, which
  `getent` would take for an option wherever it is, as `-s` choosing another
  source for every other name of the batch.
- **macOS needs none of this.** Its build uses the system C library, which
  sees directory services itself.

Besides `ssh`, Ferry runs only `getent`, and only on Linux, and on the client
`curl`, to download a release.

### mimalloc is the allocator everywhere

Every build uses mimalloc as its global allocator, on Linux, both musl and
glibc, and on macOS. Every stage of the [pipeline](pipeline.md) allocates and
frees buffers from many threads at once. musl's own allocator favours
hardening and low memory use over that: its threads largely wait on one
shared lock. mimalloc keeps free memory per thread, so threads seldom
contend. Using it everywhere gives every platform the same performance
characteristics, and one code path. Its C code is compiled into the binary,
like zstd's, so Ferry stays a single file.

**Proposed.** Once Ferry transfers files, benchmark mimalloc against jemalloc
and the system allocators, and revisit the choice if the numbers say so.

### Building and testing the musl build

- **C code needs a musl C compiler.** zstd and mimalloc compile C code into
  the binary, so building for a musl target needs musl's C toolchain, which
  CI installs as `musl-tools`.
- **CI tests the build that ships.** Tests run on the x86_64 and aarch64 musl
  targets, not only on the host's glibc, with
  `make test TARGET=<target>`.
