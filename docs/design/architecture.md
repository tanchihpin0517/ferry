# Ferry architecture

[中文](architecture.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md).

**Contents**

- [Client and servers](#client-and-servers)
- [Two peers, Source and Destination](#two-peers-source-and-destination)
- [The client makes the streams between the servers](#the-client-makes-the-streams-between-the-servers)
- [SSH](#ssh)
- [Transport](#transport)
- [Connections](#connections)
- [Installing on a remote host](#installing-on-a-remote-host)
- [Static musl builds on Linux](#static-musl-builds-on-linux)
  - [Names are looked up with `getent`](#names-are-looked-up-with-getent)
  - [mimalloc is the allocator everywhere](#mimalloc-is-the-allocator-everywhere)
  - [Building and testing the musl build](#building-and-testing-the-musl-build)
- [Packets over the session](#packets-over-the-session)

![Every transfer: the ferry client starts two ferry servers, one through each location's transport, the Source peer in one and the Destination peer in the other, and makes the streams between them, which it hands to the two; each server runs on this machine or on a host](../assets/architecture-peers.svg)

## Client and servers

**Decided.**

- **The client** is the `ferry` command you run in your terminal. It parses
  arguments, prepares, starts the two servers of a transfer, makes the
  streams between them, and shows what they report: progress, warnings, and
  the final summary. It is never a peer itself, and touches none of what one
  peer sends the other. It is not a background process: it lives as long as
  the command does.
- **A server** is a `ferry server` process that the client starts through a
  [transport](#transport): on a host over SSH, as rsync starts its remote
  side, or on this machine as a child process. Each runs one peer, and a
  [session](#transport) with the other server. It is not long-lived, listens
  on no port, and keeps no state between transfers. The only socket it
  listens on is a private Unix socket, for the length of a transfer, where
  its [streams](#connections) join it. Once its peer and its session are
  done, it says so, and ends as the client closes its
  [control connection](#transport); should that close before, the client is
  gone, and the server stops.

A transfer runs only while its client runs: the client makes the streams,
and makes again any that breaks. When the client exits, or a server's
control connection drops, the server exits too. Files already renamed into
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
the streams between the two, over which the peers talk. Where a server
runs only decides which transport starts it; nothing else differs, so
there is one code path to maintain.
Push and pull still name the direction the data goes, and no more.

**Decided.** The Destination decides what to send. The comparison runs on the
Destination, as in rsync; see [pipeline section 2](pipeline.md#2-compare).

## The client makes the streams between the servers

**Decided.** The client makes every stream between the two servers, and
hands each its end: on this machine, through the server's socket; on a
host, as the input and output of a channel over one of the client's SSH
connections. Between two hosts, it joins the two channels back to back, so
that the kernel carries the bytes from one `ssh` to the other. Over the
streams, the servers' [session](#transport) carries what one peer sends the
other; the client touches none of it. What a server reports, such as
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
- **A stream that breaks is made again, its SSH connection logged in again
  if it dropped.** The client makes another over the same SSH connection.
  Should its master be gone, it first logs in again without prompting
  (`BatchMode=yes`), which succeeds with key authentication; should the host
  refuse that, needing a password or OTP, it asks for the login in your
  terminal, the dashboard giving the terminal up meanwhile, as decided on
  2026-10-24. The transfer goes on meanwhile over the other streams, and
  the session sends again, once, what was on the broken one: nothing done
  is sent again. Without a terminal, as in a script or cron job, nothing is
  asked: the transfer goes on over the other streams, and with none left,
  the servers give up after their grace, a minute.
- **The first SSH connection carries the servers' control connections, and
  their own sessions.** Should it drop, a server it carries stops, and the
  transfer with it, said as `the SSH connection its server ran over
  dropped`. A control connection that the servers' session keeps, and that
  is joined again as a stream is, was proposed in the
  [redesign](../drafts/architecture-redesign.md), and is not built.

**Proposed.**

- **The client installs `ferry` on a host that lacks the version it needs;**
  see [Installing on a remote host](#installing-on-a-remote-host).

## Transport

![The transport: each server's peer hands its session control messages and blocks; the session sends the control messages on one stream, numbered, and the blocks in fragments on the stream with the least to write, every stream equal; the other session delivers each control message once, in order, and each block once, and says what it has; a stream that breaks, what was on it goes again on another, and the client makes another in its place](../assets/architecture-transport.svg)

**Decided.** Every byte between the client and a server, and between the
two servers, goes through one module, the **transport**. Its principles:

1. **Two kinds of connection, both abstract.** Through the transport, the
   client opens **control connections**, each to one program at the far
   end, for preparing and for following a server; see below. And it makes
   the **streams** between the two servers, which it hands to them; on
   them, each server's **session** carries what its peer sends the other's,
   as control messages, sent with `control` and delivered in order, and
   blocks, sent with `block` and delivered whole. Nothing of SSH shows, and
   the transport knows nothing of what a program is: where Ferry is
   installed, or what a token means, is for the component to say.
2. **Each control message arrives once, in order; each block once.** No
   message is lost, added, repeated, or corrupted; control messages arrive
   in the order they were sent, and blocks in any order.
3. **Its one goal is speed.** Within that guarantee, the transport moves the
   data as fast as it can.
4. **Every stream is equal.** The streams, one on each SSH connection, have
   no role of their own: the control messages go on one at a time, the
   lowest numbered; a block goes, in fragments, on the one with the least to
   write. How many there are, and which one each piece takes, is the
   transport's choice; the outside neither sees nor chooses it.
5. **Everything else happens inside.** Numbering, acknowledging, and keeping
   what was sent until the other side has it; cutting blocks into fragments
   of 64 KiB and putting them back together; and sending again, on another
   stream, what was on one that broke, are the session's work.
6. **It does not read what it carries.** Every message is the same to the
   transport; it cannot tell an error from data.
7. **It fails only when no stream comes back.** A stream that breaks is
   made again by the client, as either server tells it; the transfer goes
   on over the others meanwhile. With none left, the session waits a grace,
   a minute, for one to be joined, then gives up, and tells its peer it is
   lost, which fails the transfer. It never delivers anything in part, or
   twice.
8. **No sender waits on it, and memory is bounded.** The session takes all
   it is handed: what is kept until the other side has it is bounded by the
   peers' protocol, the blocks by the data credit, the manifest by the
   entries in flight. Each stream's writer is handed at most 256 KiB not yet
   written, so that a control message waits behind no more.
9. **Opening is preparing.** Every SSH connection is logged in before the
   server starts, and the streams are made once both servers are told to go
   on. After that, only an SSH connection that dropped needs a login again.
10. **Its health is judged in two places only.** Each session judges each
    of its streams: one silent too long is broken; and the client makes the
    streams, and makes again those that break. No other part of Ferry keeps
    a timer on a connection, and a peer learns only that the session ended,
    or was lost.

**Decided.** On a stream, control messages go ahead of the fragments of
blocks still to be written; among themselves, they keep their order, an
error with the rest. Each side sends only its first failure, once, and does
nothing more after it, however its other stages then fail; see
[pipeline.md](pipeline.md#principles). The transport reads none of what it
carries to decide what goes first: a control message is a control message.
See the [redesign](../drafts/architecture-redesign.md), whose A2 this is.

**Decided.** A **control connection** is for preparing, for the server's
life, and, while the transfer goes, for following it. It is apart from the
streams, and plain.

- **A byte stream to one program.** A control connection joins the
  component to one program on the host, its standard input and output, as
  bytes, much like a pipe. The program at the far end may be one that knows
  nothing of Ferry, such as `sh` before Ferry is installed, so the
  connection carries bytes, not messages; where messages are wanted, as for
  the handshake, the component frames them on the bytes itself. Reading
  comes to an end when the program ends; if it ends in failure, with a
  status other than 0, reading fails, rather than end as if all went well.
- **What it is for.** While preparing, the client opens control connections
  in turn, each to the program it needs: a script that probes the host, an
  install, the fixtures of `ferry test`, and last the server, over whose
  control connection the handshake goes, the builds compared, the roles, the
  options, and the session's ID, and the token by which the streams join it
  is learned, and what its root is: its device and inode, and those of each directory above it. Both
  servers then wait, having changed nothing, while the client checks their
  roots against each other: one that holds the other is refused, and a
  Destination that `sync` would empty by mistake asked about; see the
  [overview](overview.md#behaviour-of-ferry-sync). Only then does the client
  tell each to go on, with what it must never go into or delete, and a
  server whose control connection closes instead ends. All of this comes
  before any stream is made. While the transfer goes, the
  server's control connection is how the client follows it: the server
  sends there what its peer reports, its
  [metrics](profiling.md#following-a-transfer-as-it-goes) every second, and,
  once its peer is done, where its threads' time went; and it answers there
  when asked where its threads are; it says there which of its streams
  broke, what each carried, for the dashboard, and that its peer and its
  session are done. Apart from the streams, none of it waits behind the
  transfer's data.
- **Once the server has started, it goes on a channel of its own.** On a
  host, the server's output is shared with the user's shell, and with
  whatever its startup files left running, which may write there at any
  time. So once the server has been told to go on, the control connection
  moves onto a channel that reaches the server's socket as a stream does,
  over the first SSH connection. What the server's output carries from
  then on is read and dropped; its input stays open, for the server to know
  the client is there.
- **The server ends as the client closes its control connection.** Once
  its peer and its session are done, it says so, and stays, its session
  answering a stream joined after, should the other side have missed its
  end; the client closes both control connections once both servers are
  done. Closed before, it says the client is gone, and the server stops.
  Once one server is gone without saying it was done, the client stops the
  other at once: its session would only wait its grace for the other side.
- **The server is bound to the first SSH connection.** Its control
  connection, and the session it runs in, go over the first SSH connection,
  so losing that one ends the server, and the transfer. A control
  connection that the servers' session keeps, joined again as a stream is,
  was proposed in the [redesign](../drafts/architecture-redesign.md), and
  is not built.
- **Any other SSH connection that drops fails nothing.** Its streams are
  made again, the connection logged in again first, and the session sends
  again what was on them; see [SSH](#ssh) and [Connections](#connections).
- **It is plain.** Its bytes go as they are, neither numbered nor sent
  again: the principles of speed are not for it, since it carries little:
  before the transfer, and during it, a few hundred bytes a second.
- **A peer's failure is told on it too.** The server says it as its peer
  fails, and the client takes the first either server says as the
  transfer's cause; the other peer learns it from its session, as a control
  message in order with the rest.
- **Where it runs is the transport's.** Like everything below, which SSH
  connection carries it is hidden. It shares the first with a stream, so it
  needs no login of its own and is not counted by `--ssh-connections`.
- **A stream reaches a socket.** Each stream, as the control connection's
  channel does, reaches the server's socket: straight, as a channel SSH
  forwards to it; with `--remote-join`, by running `ferry server --join
  <token>` there; or, on this machine, handed over through it. What the
  server reads first says what it is: a stream, and its number, the same at
  both servers, or the control connection's channel.
- **The server's answer goes between marks.** On a host, a program runs
  through the user's login shell, whose startup files may print to
  standard output, as a tool that warns there does, and may leave a
  program running that prints later. The server sends one message there,
  its answer to the handshake, and writes it between a start mark,
  `\0ferry: start\0`, and an end mark, `\0ferry: end\0`, in one write of a
  few hundred bytes, which a pipe keeps whole: whatever else is printed
  comes before or after it, never inside. The client skips what comes
  before the start mark rather than reading it as packets, and says once
  what the shell printed, for its startup files to be fixed; an answer
  without its end mark was cut into, and fails the handshake. A program
  whose output ends before the start mark did not start. The server
  answers a stream or channel that reaches its socket straight with the
  start mark, and a `--join` process writes it as it starts, after what the
  shell printed; where the other server is on this machine, the client
  writes it, as that one writes nothing before its session does. So each
  stream begins with one start mark each way, which the server reading it
  skips to.


**Decided.** There is more than one transport, and the client does not know
which one it has. For each location, it asks for the transport of that
location, and uses only the two kinds of connection.

- **The SSH transport** reaches a host, as this section and
  [Connections](#connections) describe.
- **The local transport** reaches this machine. A control connection starts
  its program as a child process, joined by pipes. Its streams are socket
  pairs, whose ends the client hands to the servers through their sockets.
  There is nothing to log in to, probe, or install: the server is the
  client's own binary.
- **Another way to reach a host** is another transport. Nothing above the
  transport changes for it.

**Proposed.** The transport chooses how many streams, each a channel,
each SSH connection carries. A channel of an SSH connection can have only
so much data unacknowledged, its window, 2 MiB in OpenSSH (`CHAN_SES_WINDOW_DEFAULT`),
so it carries at most its window per round trip: about 20 MB/s at 100 ms. A
TCP connection, for its part, is held back by congestion control and losses, whatever its
channels. With C connections of K channels each, a transfer reaches at most
the least of the link's capacity, C TCP connections' throughput, and C × K
windows per round trip. The SSH connections, from `--ssh-connections`, deal
with TCP's limit; the channels, which need no login, deal with the window's. The
transport would measure the round trip during the handshake and open
enough channels that the window is not the limit: one on a short path, a
few across continents. To be decided once it can be measured on a path
with a long round trip.

## Connections

![Connections to a host: the ferry client runs one ssh master per SSH connection, each with its own TCP connection; over the first run the control connections, which start the ferry server, the server's own once it started, and a stream; over the others, a stream each; every stream is a channel sshd forwards to the server's Unix socket, with no shell, whose other end the client hands to the other server](../assets/architecture-connections.svg)

**Decided.** This is how the [transport](#transport) uses SSH. One `ssh`
master per SSH connection, and one TCP connection per master. Over them run
the [control connections](#transport), one of which runs the server, and
the streams, one on each, all equal, which join it. Every SSH connection is
logged in before the server starts.

- **One SSH connection by default, sharing your own ControlMaster if you
  have one.** The client reads the options that apply to the host from
  `ssh -G <host>`. If they set up a ControlMaster (`controlmaster` is not
  `false` and `controlpath` is not `none`), it adds no control options, and
  the first SSH connection shares your master. Otherwise it uses one of its
  own: `ControlMaster=auto`, a `ControlPath` under `/tmp/ferry-<uid>/ssh/`,
  and `ControlPersist=600`, with `ServerAliveInterval=15` and
  `ServerAliveCountMax=4`, so that it ends, and every session on it, once the
  host has not answered for about a minute: preparing, as an upload, has no
  session yet to watch it, and would otherwise wait on a connection that is
  gone until TCP gives up, for hours. Your own master keeps the options you gave it.
  Either way a login or OTP is typed once per host, and a later run within
  the persist time needs none. The control
  sockets live under `/tmp` because a Unix socket path is limited to 103
  bytes on macOS. No setting turns this off: a ControlMaster of your own is
  found without one. Every `ssh` Ferry runs also gets `RemoteCommand=none`,
  `ClearAllForwardings=yes`, `PermitLocalCommand=no`, and `ForwardX11=no`,
  as its sessions run their own commands and need no forwarding, and
  `ConnectTimeout=10` unless `ssh -G` shows one of yours.
- **`--ssh-connections N` opens N SSH connections to a host, each its own
  TCP connection.** The default is 1. N is exact: SSH connections are
  opened while preparing, so a transfer starts with all N or not at all, as
  below. It counts SSH connections only; how many channels each carries, and
  which carries the control connections, is the transport's choice. The name
  says SSH, because a connection outside the transport is something else,
  and because each one may cost a login. A master carries all its sessions
  over one TCP connection, so more sessions on it are no faster. On a lossy
  path a single TCP flow is held well below the link's capacity by
  congestion control; to one host with about 0.01% loss, one connection
  reached about 31 MB/s and four together about 60 MB/s, the link's limit.
  Each SSH connection after the first has a master of Ferry's own,
  `/tmp/ferry-<uid>/ssh/%C-1`, `%C-2`, and so on, kept for the persist time
  like the first.
- **SSH connections are opened first, then the server is started.** As the
  [overview](overview.md#reliability-comes-first-then-speed) requires, the
  transport first opens every SSH connection: it starts each master and
  waits until it is logged in. Only then does the client, over the control
  connection, probe the host, install Ferry if needed, and start the server,
  so nothing after the logins asks for anything.
- **An SSH connection is opened with no task.** For each SSH connection, the
  transport runs `ssh -N`, which logs in and starts the master but runs
  nothing on the host, and holds it for the whole run. Opening an SSH
  connection is only logging in; the control connections and the streams
  are sessions opened later over masters already logged in, with no login
  of their own. Holding the master also keeps it alive when it is your own
  ControlMaster without `ControlPersist`, which would otherwise end with the
  first `ssh` that used it. The `ssh -N` ends with Ferry, however it ends,
  `SIGTERM` and `SIGHUP` too, which end it before Ferry exits; only
  `SIGKILL` leaves it, as nothing then runs.
- **SSH connections log in one at a time.**
  Each after the first tries `BatchMode=yes`, which succeeds with key
  authentication or a master still alive. If that fails, the client says
  which one is logging in, such as `logging in SSH connection 2 of 4 to
  host`, and lets `ssh` ask in the terminal, one SSH connection after
  another, so that prompts never interleave and the progress line never
  cuts into them. A host that accepts a TOTP code only once makes each login
  wait for the next code.
- **If any SSH connection cannot be opened, preparing fails.** A failed
  login, one that needs a terminal when there is none, or Ctrl-C during a
  login stops the run before the transfer starts, with nothing to clean up,
  and says which SSH connection failed and why, such as `ferry: fatcat: SSH
  connection 3 of 4 needs a login, and there is no terminal to type it in`.
  Asked for N SSH connections, Ferry never quietly transfers over fewer.
  Once the transfer goes, one that drops is logged in again, as a stream
  over it is made again; see [SSH](#ssh).
- **One stream on each SSH connection.** A transfer has as many streams
  as the side with the more SSH connections, one where both are on this
  machine. The client numbers them, the same at both servers: stream `n`
  goes over SSH connection `n` modulo the count, on each side.
- **Every stream reaches its servers alike, with no shell.** Once a server
  is running, it waits on a Unix socket in a private directory on the
  host, named by a random token, both learned over its control connection.
  On a host, a stream's end is a channel that SSH forwards to that socket,
  as `ssh -W <socket>` opens: no shell runs for it, so nothing a shell or
  what it leaves running prints can come in between. No port is opened. The
  client makes each stream as a socket pair: one end is the channel's input
  and output, the other is handed to the server on this machine, or is the
  input and output of the other host's channel, so that the two `ssh`
  processes are joined back to back.
- **The client writes what each server reads first.** Into each end, before
  it hands either over: for a channel that comes straight, the byte that
  says so; the header, which says it is a stream, and its number; and,
  where the other server is on this machine, the start mark. A server on a
  host answers a channel that comes straight with the start mark; so each
  stream begins with one start mark each way, which the server reading it
  skips to.
- **Where SSH will not forward to a socket, `--remote-join` has streams join
  instead.** A host's `sshd` may forbid it, with
  `AllowStreamLocalForwarding no`; the control connection's channel, opened
  first, then does not get through, and preparing fails, naming
  `--remote-join`. With it, every stream's channel, and the control
  connection's, runs `ferry server --join <token>` on the host instead,
  which writes the start mark and hands its standard input and output to
  the server through the socket. Such a channel runs through the user's
  shell, and only what the shell prints before the start mark is kept out:
  what it leaves running may still write into it. So it is not done
  unasked.
- **Each session judges each of its streams.** A stream that ends, fails, or
  on which nothing comes for 20 seconds is broken: the session shuts it
  down both ways, so that its other end sees it broken too, sends again on
  another what was on it, and tells the client, which makes another in its
  place. A stream on which nothing was written for 5 seconds is told it is
  alive. The session reads every stream dry, what comes bounded by the
  peers' credit, so a slow link, its data still coming, is never taken for
  a silent one. With no stream left, the session waits a grace of a minute
  for one to be joined, then gives up. These three times are the only
  timers on a connection, but for the client's own while it logs in, and
  for each server at its end.
- **A stream is made again over the same SSH connections.** Under the next
  number never used whose SSH connections are the broken one's, on both
  sides, and only for one that both servers were handed, once, however
  many servers say it broke; one that cannot be made yet is tried again 2
  s after. A dropped SSH connection is logged in again first; see
  [SSH](#ssh).
- **What each SSH connection carries is told for the dashboard.** Each
  server says every second what each of its streams carried, both ways,
  and how long since anything came on it; the client adds up the streams
  of each SSH connection, for the
  [dashboard](dashboard.md#what-it-shows)'s rate and "heard".

**Not chosen.**

- **More sessions, or forwarded ports, over one master.** SSH carries every
  session and every forwarded port of a master as channels over its one TCP
  connection, so they share one flow and its congestion control. Four
  sessions over one master were no faster than one.
- **A TCP port of Ferry's own.** Connecting straight to a port the server
  opens would give several flows without more logins. But it needs
  encryption of Ferry's own, with a key passed over SSH, a port open on the
  host, and a fallback to SSH where a firewall blocks it. Extra SSH
  connections give the same flows with SSH's own encryption and nothing
  else to maintain.

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
- **A release build installs a binary.** The client downloads the release of
  its own version for the host's OS and architecture from GitHub Releases
  with `curl`, checks it against `SHA256SUMS`, caches it locally, and uploads
  it over the SSH connection. The host needs no outbound network access, which cluster
  compute nodes often lack. Linux binaries are
  [static musl builds](#static-musl-builds-on-linux), so they run on any
  distribution's C library. The host puts the upload in place, by a rename,
  only once it has checked it: its size, and its SHA-256 where the host has
  `sha256sum` or `shasum`; then the binary must run there, and say, as
  `ferry build-id`, that it is the build it is installed as. So an upload cut
  short, by Ctrl-C or a dropped connection, installs nothing, and a binary
  that cannot run there, as where the home directory is mounted `noexec`, is
  found at install. Each install writes a file of its own, so two clients
  installing at once do not mix their uploads.
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

## Packets over the session

**Decided.** A peer reads and writes a sequence of typed **packets**:
`[type][length][payload]`. Its server carries them over the
[session](#transport): the blocks of files, each put back together whole
from its data packets, as the session's blocks, delivered once, in any
order, and every other packet as a control message, delivered once, in
order. Each kind of message has its own type:

| Type | Direction | Content |
|---|---|---|
| control | both | handshake (versions, capabilities, options), errors, warnings |
| report | server → client, on its control connection | progress counts, how full the pipeline's queues are, paths a dry run lists, warnings, what could not be read or copied, which streams broke and what each carried, for the client to show |
| manifest | Source → Destination | the Source's files: paths and metadata |
| needs | Destination → Source | the files the Destination needs |
| data | Source → Destination | blocks of files |
| file hash | Source → Destination | the hash of a file sent in segments, merged from theirs |
| credit | Destination → Source | the data credit, given as it starts, and given back as blocks are written |
| results | Destination → Source | each file's success or failure |
| blocks' end | Source's server → Destination's server | how many blocks the Source sent, in place of the end that follows the last: the Destination's server hands its peer that end once that many have come |

Packets of different types are interleaved, so that kinds of message that
flow at the same time do not wait for each other. While data is flowing, the
Source keeps sending manifest packets as its scan finds files, and the
Destination keeps sending needs and results. One pump thread per peer
takes packets from three queues, each in the order it was handed over.
Messages go first: needs, results, and control messages, such as an error,
and the Destination's `Done`, which must follow the results. Then, between
the manifest and data, data goes first while its queue is at least a
quarter full, so that the sender's files are not held back behind a
manifest that a scan makes faster than the streams carry it; below that,
the manifest goes first, so that the Destination finds more to need before
the sender runs out. The data queue is bounded, and so is what the Source
sends of the data, by the credit, so a Source faster than the streams waits
instead of piling the data up in memory. Each peer learns that the other is
done as its input ends: its server ends it once the session says the other
is done, all it sent delivered.
