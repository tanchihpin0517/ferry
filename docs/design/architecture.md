# Ferry architecture

[中文](architecture.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md).

**Contents**

- [Client and servers](#client-and-servers)
- [Two peers, Source and Destination](#two-peers-source-and-destination)
- [The client makes the stream between the servers](#the-client-makes-the-stream-between-the-servers)
- [SSH](#ssh)
- [Transport](#transport)
- [Connections](#connections)
- [Installing on a remote host](#installing-on-a-remote-host)
- [Static musl builds on Linux](#static-musl-builds-on-linux)
  - [Names are looked up with `getent`](#names-are-looked-up-with-getent)
  - [mimalloc is the allocator everywhere](#mimalloc-is-the-allocator-everywhere)
  - [Building and testing the musl build](#building-and-testing-the-musl-build)
- [Packets over the stream](#packets-over-the-stream)

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
  side, there by Ferry's launcher, or on this machine as a child process.
  Each runs one peer, over the [stream](#transport) to the other server. It
  is not long-lived, and keeps no state between transfers. The only socket
  it listens on is a private Unix socket, for the length of a transfer,
  where the [stream](#connections) reaches it; where SSH forwards to no
  socket, a port on loopback instead, which the stream reaches with a
  secret. Once its peer is done, it says so, and ends as the client closes
  its [control connection](#transport), or once it has waited 20 seconds for
  that; should that close before, the client is gone, and the server stops.

A transfer runs only while its client runs, and while its stream does:
should the stream break, or an SSH connection drop, the transfer fails, as
with rsync, and nothing makes it again. When the client exits, or a
server's control connection drops, the server exits too. Files already
renamed into place are complete, so the next run skips them; a file that
was not finished is sent again from the start.

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

## The client makes the stream between the servers

**Decided.** The client makes the stream between the two servers, and
hands each its end: on this machine, through the server's socket; on a
host, as the input and output of a channel over the host's SSH connection.
Between two hosts, it joins the two channels back to back, so that the
kernel carries the bytes from one `ssh` to the other. The stream carries
what one peer sends the other, as it is, with a [thin layer](#transport) of
its own; the client touches none of it. What a server reports, such as
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
  launcher; the servers the launcher starts, and their control connections;
  and the stream. A host name may lead to several machines, as a cluster's
  login nodes behind one name do; over one connection, everything reaches
  the same one. Should the connection drop, or the stream break, the
  transfer fails, as with rsync, said as `the SSH connection its server ran
  over dropped`, or why the stream broke. Nothing logs in again, or makes
  the stream again, as decided on 2026-10-25, so as to keep SSH's
  uncertainty out of the transfer; see the
  [redesign](../drafts/architecture-redesign.md#2026-10-25-重新決定一條-ssh-連線斷線就失敗).
  Files already renamed into place stay, and the next run skips them.

**Proposed.**

- **The client installs `ferry` on a host that lacks the version it needs;**
  see [Installing on a remote host](#installing-on-a-remote-host).

## Transport

![The transport: each server's peer writes its packets on the one stream, as they are, and the other's reads them, in order; the stream's writer adds a keepalive once nothing went on it for 5 seconds, between two packets, and its end once its peer is done; its reader takes both out, and takes the stream for broken once nothing came for 20 seconds, or once it ends without its end, which fails the transfer](../assets/architecture-transport.svg)

**Decided.** Every byte between the client and a server, and between the
two servers, goes through one module, the **transport**. Its principles:

1. **Two kinds of connection, both abstract.** Through the transport, the
   client opens **control connections**, each to one program at the far
   end, for preparing and for following a server; see below. And it makes
   the **stream** between the two servers, which it hands to them; on it,
   each peer writes its packets, and the other reads them, as they are.
   Nothing of SSH shows, and the transport knows nothing of what a program
   is: where Ferry is installed, or what a token means, is for the
   component to say.
2. **Each packet arrives once, in order, whole.** What one peer writes, the
   other reads, in the order written: nothing lost, added, repeated, or
   corrupted, as a byte stream gives it. Or the stream breaks, and the
   transfer fails.
3. **Its one goal is speed.** Within that guarantee, the transport moves the
   data as fast as it can: nothing is numbered, acknowledged, or kept on
   the way, and the peers write straight onto the stream.
4. **There is one stream.** A host has one SSH connection, and the stream
   is a channel of it, or, between two hosts, a channel of each, joined; with
   both servers on this machine, a socket pair. More channels over the same
   connection, for a path whose round trip one channel's window holds back,
   are an [improvement option](../drafts/architecture-redesign.md#改進選項),
   not built.
5. **What it adds, it adds inside.** The stream's writer sends a keepalive
   once nothing went on the stream for 5 seconds, only between two packets,
   and the stream's end once its peer is done; its reader takes both out,
   so that no peer sees them.
6. **It does not read what it carries.** Every packet is the same to the
   transport, but for its type and length, which say where it ends; it
   cannot tell an error from data.
7. **It fails as the stream breaks.** The reader takes the stream for
   broken once nothing came on it for 20 seconds, or once it ends without
   its end; the peer reading it then fails, and the transfer with it. A
   writer waiting for room stops once the stream is taken for broken.
   Nothing makes the stream again: a transfer cut short is run again, and
   skips what was done.
8. **No sender waits on it, but for the stream itself, and memory is
   bounded.** A peer's pump writes its packets straight onto the stream,
   and waits only for the stream to take them. What waits to be written is
   bounded by the peers' protocol: the blocks by the data credit, the
   manifest by the entries in flight.
9. **Opening is preparing.** Every SSH connection is logged in before the
   server starts, and the stream is made once both servers are told to go
   on. After that, nothing logs in.
10. **Its health is judged in two places only.** Each server's reader judges
    the stream: silent for 20 seconds, it is broken. And the client judges
    each server by its control connection: silent for 20 seconds, the server
    is gone, and the transfer fails. The connection the client follows a
    host's launcher by goes over the stream layer too, and is judged as the
    stream is, at both its ends. No other part of Ferry keeps a timer on a
    connection.

**Decided.** On the stream, each peer's pump puts its messages, an error
among them, ahead of data waiting to be written; among themselves, they
keep their order; see [Packets over the stream](#packets-over-the-stream).
Each side sends only its first failure, once, and does nothing more after
it, however its other stages then fail; see
[pipeline.md](pipeline.md#principles). The transport reads none of what it
carries to decide what goes first. A session of the servers' own, which
numbered and acknowledged what went over several streams, and sent again
what was on one that broke, was built in A2 and taken out on 2026-10-25,
with the reconnecting it was for; see the
[redesign](../drafts/architecture-redesign.md#2026-10-25-重新決定一條-ssh-連線斷線就失敗).

**Decided.** A **control connection** is for preparing, for the server's
life, and, while the transfer goes, for following it. It is apart from the
stream, and plain.

- **A byte stream to one program.** A control connection joins the
  component to one program on the host, its standard input and output, as
  bytes, much like a pipe. The program at the far end may be one that knows
  nothing of Ferry, such as `sh` before Ferry is installed, so the
  connection carries bytes, not messages; where messages are wanted, as for
  the handshake, the component frames them on the bytes itself. Reading
  comes to an end when the program ends; if it ends in failure, with a
  status other than 0, reading fails, rather than end as if all went well.
- **What it is for.** While preparing, the client opens control connections
  in turn, each to the program it needs: on a host, its first session, a
  script that probes the host, installs this build there if it must, and
  becomes Ferry's launcher; then, through the launcher, sudo's checks, the
  fixtures of `ferry test`, and last the server, over whose control
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
  must, and then becomes **Ferry's launcher**, `ferry launcher`, which
  listens on a socket of its own, as a server does, and says where in the
  session's output. Everything else on the host goes over channels to it,
  which run no shell. The client follows the launcher on the first, and the
  launcher lives as long as that channel, or as its session's output, which
  the client holds. Once the launcher knows what that channel is for, both
  sides go on it over the stream layer, its keepalives and its end: should
  nothing come on it for 20 seconds, as when the SSH connection dropped
  without a word, the launcher ends, and the client takes it for gone,
  rather than waiting for `sshd` to find that, which with its defaults takes
  over two hours. A connection to it on which nothing comes first, neither
  its first byte nor its first packet, is let go after 20 seconds, so that
  it holds up none after it, as the launcher takes them in turn. On each
  other, the client has it start one program, which then has that channel as
  its standard input and output: sudo's checks, for `--remote-sudo`, a
  fixture of `ferry test`, or a server, through `sudo` with `--remote-sudo`.
  The launcher says on the channel it is followed by how each ended, and
  what one run to its end, as `sudo`, said on standard error, for the client
  to show. It only starts things: what it starts does not depend on it, and
  once it has started one, it reads nothing more on its channel. See the
  [draft](../drafts/one-session.md).
- **A server's control connection is the channel it was started on.** No
  shell can write to it, so the server shakes hands there, and its control
  connection stays there until it ends. On this machine, the client starts
  the server itself, through pipes.
- **The server ends as the client closes its control connection.** Once
  its peer is done, it says so, and waits for the client to close its
  control connection, for 20 seconds at most; the client closes both once
  both servers are done. Closed before, it says the client is gone, and the
  server stops. Once one server is done, the other is given until its
  stream would be taken for broken; once one has failed, both are given 5
  seconds; once one is gone without saying it was done, the other is
  stopped at once, as are the `ssh` processes that carry the stream.
- **The client judges each server by it.** Every second, the server says
  on it what the stream carried, so something comes on it while the server
  is there. Should nothing come for 20 seconds, as when the host stopped
  answering, or the server or its `ssh` was stopped, the client takes the
  server for gone, and says so, as `nothing came on its control connection
  for 20 s`; the transfer fails.
- **The server is bound to its host's SSH connection.** Its control
  connection and the stream go over it, so losing it ends the server, and
  the transfer.
- **It is plain.** Its bytes go as they are, neither numbered nor sent
  again: the principles of speed are not for it, since it carries little:
  before the transfer, and during it, a few hundred bytes a second.
- **A peer's failure is told on it too.** The server says it as its peer
  fails, and the client takes the first either server says as the
  transfer's cause; the other peer learns it from the stream, as a message
  in order with the rest.
- **Where it runs is the transport's.** Like everything below, it is
  hidden. It shares the host's SSH connection with the stream, so it needs
  no login of its own.
- **The stream reaches a socket.** The stream, as each channel to the
  launcher does, reaches the server's socket: straight, as a channel SSH
  forwards to it; where SSH forwards to no socket, at a port on loopback
  instead, giving a secret first; with `--remote-join`, by running `ferry
  server --join <token>` there; or, on this machine, handed over through
  it; see [Connections](#connections). Only the stream comes to a server's
  socket, and what it reads first says so; on the launcher's, what comes
  first says what the connection is for.
- **What Ferry says in a session goes in marked lines, and the server's
  answer between marks.** On a host, the first session runs through the
  user's login shell, whose startup files may print to standard output, as
  a tool that warns there does, and may leave a program running that
  prints later. So each line the session's script, and then the launcher,
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
  launcher's channel carries nothing else, but one that joins through the
  shell may carry what the shell left running prints. The client skips
  what comes before the start mark rather than reading it as packets; an
  answer without its end mark was cut into, and fails the handshake. A
  program whose output ends before the start mark did not start. What
  listens on a socket, the launcher or a server, answers a channel that
  reaches it straight, or by its port, with the start mark, and a `--join`
  process writes it as it starts, after what the shell printed; where the
  other server is on this machine, the client writes it, as that one
  writes nothing before its peer's packets. So the stream begins with one
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

**Not built.** A channel of an SSH connection can have only so much data
unacknowledged, its window, 2 MiB in OpenSSH (`CHAN_SES_WINDOW_DEFAULT`),
so it carries at most its window per round trip: about 20 MB/s at 100 ms. A
TCP connection, for its part, is held back by congestion control and
losses, whatever its channels. On a path with a long round trip, the stream
could go over several channels of the one SSH connection, which need no
login, so that the window is not the limit; any of them breaking would
fail the transfer, as the stream does now. Measured on 2026-10-23, it was
no faster to fatcat (1.7 ms) or nano4 (6.5 ms); it waits for a path with a
long round trip to be measured on, as an
[improvement option](../drafts/architecture-redesign.md#改進選項).

## Connections

![Connections to a host: the ferry client runs one ssh master for the host, your own ControlMaster or Ferry's, with one TCP connection; over it run the host's one session, through the login shell, which probes the host, installs Ferry, and becomes Ferry's launcher; channels sshd forwards to the launcher's socket, with no shell, one that follows it, and on each other a server the launcher starts, whose control connection it is; and the transfer's one stream, a channel sshd forwards to the server's socket, whose other end the client hands to the other server; should it drop, the transfer fails](../assets/architecture-connections.svg)

**Decided.** This is how the [transport](#transport) uses SSH. One `ssh`
master per host, and one TCP connection. Over it run the host's one
session, which becomes Ferry's launcher; the channels to the launcher, each
server's [control connection](#transport) among them; and the stream,
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
  launcher, which starts the server, so nothing after the login asks for
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
- **One stream.** A transfer has one stream, which the client makes once
  both servers are told to go on: over the host's SSH connection to a
  server on a host, or a socket pair to one on this machine.
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
  says so, or, for one to a port, its secret; the header, which says it is
  the stream; and, where the other server is on this machine, the start
  mark. A server on a host answers a channel that comes straight, or by its
  port, with the start mark; so the stream begins with one start mark each
  way, which the server reading it skips to.
- **Where SSH will not forward to a socket, a port on loopback; where it
  forwards to nothing, `--remote-join`.** A host's `sshd` may forbid
  forwarding to a socket, with `AllowStreamLocalForwarding no`, and one
  before OpenSSH 6.7 cannot do it, though either may forward to a port. So
  the launcher listens on a port on loopback too, and says it, with a
  secret of 64 hexadecimal digits, beside its socket; the client tries the
  socket first, then the port, and the first channel that gets through
  decides: the launcher closes the other, and the servers it starts listen
  only on the one that got through. A port is anyone's on the host to
  connect to, so each connection to one gives the secret first, which the
  client learned over the first session, or the server's control
  connection; one that gives another, or nothing, is let go of, and at
  most 16 are waited on at once. Where neither gets through, preparing
  fails, naming `--remote-join`. With it, each channel to the launcher,
  and the stream's, run `ferry server --join <token>` on the host instead,
  which writes the start mark and hands its standard input and output to
  the launcher, or the server, through its socket. Such a channel runs
  through the user's shell, and only what the shell prints before the
  start mark is kept out: what it leaves running may still write into it.
  So it is not done unasked, and it costs a session each. A joining
  process stays until what it joined is done with what it handed over, as
  sshd closes a session's input once its process exits; so the stream ends
  with its end, not as the channel closes, and the launcher stays as long
  as it is followed.
- **Each server's reader judges the stream.** The stream on which nothing
  comes for 20 seconds, or which ends without its end, is broken: the reader
  shuts it down both ways, where it is a socket, so that its other end sees
  it broken too, and the peer reading it fails, and the transfer with it. A
  stream on which nothing was written for 5 seconds is told it is alive, so
  a slow link, its data still coming, is never taken for a silent one. These
  two times, which the connection a launcher is followed by keeps too, and
  the client's 20 seconds on each control connection, are the only timers on
  a connection, but for the client's own while it logs in, and while it
  hands a server on this machine its end of the stream, each server's wait
  for its control connection to close once it is done, and the 20 seconds
  the launcher, and a server, give what comes first on a connection to its
  socket.
- **What the SSH connection carries is told for the dashboard.** Each
  server says every second what the stream carried, both ways, and how
  long since anything came on it; the client shows it for the host's SSH
  connection, as the [dashboard](dashboard.md#what-it-shows)'s rate and
  "heard".

**Not chosen.**

- **Several SSH connections to a host** (`--ssh-connections`, built in A2
  and removed on 2026-10-25). On a lossy path a single TCP flow is held
  well below the link's capacity by congestion control: to one host with
  about 0.01% loss, one connection reached about 31 MB/s, and four together
  about 60 MB/s, the link's limit. But each connection may land on another
  machine behind the host's name, each costs a login, an OTP each where the
  host asks for one, and spreading one transfer over them needed a session
  of the servers' own; see the
  [redesign](../drafts/architecture-redesign.md#2026-10-25-重新決定一條-ssh-連線斷線就失敗).
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
  before the session becomes Ferry's launcher, so it costs no session of
  its own. An upload, of a release's binary or a dev build's source, is the
  last thing the session reads: what installs it reads it to its end, and
  then becomes the launcher itself.
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

## Packets over the stream

**Decided.** A peer reads and writes a sequence of typed **packets**:
`[type][length][payload]`, the type a byte, and the length four,
little-endian. Its server carries them over the [stream](#transport), as
they are, in order, with two packets of the stream's own, which no peer
sees. Each kind of message has its own type:

| Type | Direction | Content |
|---|---|---|
| control | both | handshake (versions, capabilities, options), errors, warnings |
| report | server → client, on its control connection | progress counts, how full the pipeline's queues are, paths a dry run lists, warnings, what could not be read or copied, what the stream carried, for the client to show |
| manifest | Source → Destination | the Source's files: paths and metadata |
| needs | Destination → Source | the files the Destination needs |
| data | Source → Destination | blocks of files |
| file hash | Source → Destination | the hash of a file sent in segments, merged from theirs |
| credit | Destination → Source | the data credit, given as it starts, and given back as blocks are written |
| results | Destination → Source | each file's success or failure |
| keepalive | each server → the other, on the stream; the client and a launcher, each to the other, on the connection it is followed by | nothing: sent once nothing went on the stream for 5 seconds, between two packets, and taken out by the reader |
| end | each server → the other, on the stream; the client → a launcher, on the connection it is followed by | nothing: the stream's end, sent once the peer is done; the reader ends there, and takes a stream that ends without it for broken |

Packets of different types are interleaved, so that kinds of message that
flow at the same time do not wait for each other. While data is flowing, the
Source keeps sending manifest packets as its scan finds files, and the
Destination keeps sending needs and results. One pump thread per peer
takes packets from three queues, each in the order it was handed over.
Messages go first: needs, results, and control messages, such as an error,
and the Destination's `Done`, which must follow the results. Then, between
the manifest and data, data goes first while its queue is at least a
quarter full, so that the sender's files are not held back behind a
manifest that a scan makes faster than the stream carries it; below that,
the manifest goes first, so that the Destination finds more to need before
the sender runs out. The data queue is bounded, and so is what the Source
sends of the data, by the credit, so a Source faster than the stream waits
instead of piling the data up in memory. Each peer learns that the other is
done as its input ends: at the stream's end, which the other's server
writes once its peer is done, after all it wrote.
