# Ferry architecture

[中文](architecture.zh-TW.md)

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md).

**Contents**

- [Client and servers](#client-and-servers)
- [Two peers, Source and Destination](#two-peers-source-and-destination)
- [The client relays between the servers](#the-client-relays-between-the-servers)
- [SSH](#ssh)
- [Transport](#transport)
- [Connections](#connections)
- [Installing on a remote host](#installing-on-a-remote-host)
- [Static musl builds on Linux](#static-musl-builds-on-linux)
  - [Names are looked up with `getent`](#names-are-looked-up-with-getent)
  - [mimalloc is the allocator everywhere](#mimalloc-is-the-allocator-everywhere)
  - [Building and testing the musl build](#building-and-testing-the-musl-build)
- [Packets on one connection](#packets-on-one-connection)

![Every transfer: the ferry client in the middle starts two ferry servers, one through each location's transport, the Source peer in one and the Destination peer in the other, and relays between them; each server runs on this machine or on a host](../assets/architecture-peers.svg)

## Client and servers

**Decided.**

- **The client** is the `ferry` command you run in your terminal. It parses
  arguments, prepares, starts the two servers of a transfer, relays between
  them, and shows what they report: progress, warnings, and the final
  summary. It is never a peer itself. It is not a background process: it
  lives as long as the command does.
- **A server** is a `ferry server` process that the client starts through a
  [transport](#transport): on a host over SSH, as rsync starts its remote
  side, or on this machine as a child process. Each runs one peer. It is not
  long-lived, listens on no port, and keeps no state between transfers. The
  only socket it listens on is a private Unix socket, for the length of a
  transfer, where its [paths](#connections) join it. It lives as long as its
  [control connection](#transport).

A transfer runs only while its client runs, because the client is always
between its two ends. When the client exits or a server's control connection
drops, the server exits too. Files already renamed into place are complete,
so the next run skips them; a file that was not finished is sent again from
the start.

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
gets a transport for its location, starts a server through it, and the peers
talk through the client. Where a server runs only decides which transport
starts it; nothing else differs, so there is one code path to maintain.
Push and pull still name the direction the data goes, and no more.

**Decided.** The Destination decides what to send. The comparison runs on the
Destination, as in rsync; see [pipeline section 2](pipeline.md#2-compare).

## The client relays between the servers

**Decided.** The client opens a [connection](#transport) to each server and
passes on what one sends to the other. It reads only the type of each
message: a report, such as progress, a path a dry run lists, or a warning, is
for the client, which shows it; anything else it sends on as it is, without
decoding it. The two machines need not reach each other, and no credentials
are placed on either of them.

The cost is that every message is received whole by the client and sent
again. Between two servers on this machine, or between one here and one on a
host, that is one more pass through a local pipe, far faster than the
network or the disk. From remote to remote, the data also crosses the local
machine's network link twice.

## SSH

**Decided.**

- **Ferry runs the system's `ssh`**, as rsync and File Peeker do, rather than
  an SSH library. `~/.ssh/config`, `ProxyJump`, host aliases, ssh-agent,
  Kerberos, hardware keys, OTP, and `known_hosts` all work as they do for
  `ssh` itself. A library would have to reimplement each of them, and a host
  that `ssh` reaches but Ferry cannot is the failure this avoids.
- **The client holds the connection and authenticates in your terminal.** A
  password or OTP is typed where you ran `ferry`.
- **A connection that drops is retried, then waits for a login.** The client
  first reconnects without prompting (`BatchMode=yes`), which succeeds with
  key authentication. If the host needs a password or OTP, the transfer
  pauses and the client asks for the login in your terminal. Once you log in,
  the client starts a new server and the transfer continues. Files that were
  not finished are sent again from the start.
**Proposed.**

- **Without a terminal, a drop fails the transfer** instead of waiting for a
  login nobody can type, as in a script or cron job. A rerun skips the files
  already complete.
- **The client installs `ferry` on a host that lacks the version it needs;**
  see [Installing on a remote host](#installing-on-a-remote-host).

## Transport

![The transport: the client and a server see one connection, where one sends messages and the other receives them whole and in order; underneath, the sending side's transport cuts messages into numbered pieces and gives each to the path with the shortest queue, every path equal, and the receiving side's transport puts them back together in order](../assets/architecture-transport.svg)

**Decided.** Every byte between the client and a server goes through one
module, the **transport**. Its principles:

1. **Two kinds of connection to the outside, both abstract.** Through the
   transport, a component opens a **connection**, then sends messages with
   `send` and receives them with `recv`; the principles below are about
   these. Or it opens a **control connection**, for preparing; see below.
   Either way, the component names the program at the far end, a command on
   the host, and gets a connection to it. Nothing of SSH shows, and the
   transport knows nothing of what the program is: where Ferry is installed,
   or what a token means, is for the component to say.
2. **Messages arrive whole and in order.** No message is lost, added,
   repeated, or corrupted, and messages arrive in the order they were sent.
3. **Its one goal is speed.** Within that guarantee, the transport moves the
   data as fast as it can.
4. **Every path below is equal.** The SSH connections and channels the
   transport uses are all alike, with no role of their own: none is for
   control, none for data. How many there are, and which one each piece
   takes, is the transport's choice; the outside neither sees nor chooses
   it. Beside its path, each SSH connection has a heartbeat channel, which
   is not a path: it carries no messages, only heartbeats; see
   [Connections](#connections).
5. **Everything else happens inside.** Cutting messages into pieces, sending
   the pieces over the paths, and putting them back together and in order on
   the far side are the transport's work.
6. **It does not read what it carries.** Every message is the same to the
   transport; it cannot tell an error from data.
7. **It fails rather than deliver anything wrong.** If the guarantee cannot
   be kept, such as when a path drops and cannot be restored, `send` and
   `recv` fail. A message is never delivered in part or out of order. A
   path ends with an end marker, so that a path cut short, even between two
   pieces, is told from one closed.
8. **A sender waits, and memory is bounded.** When the far side cannot keep
   up, `send` waits, instead of piling messages up in memory.
9. **Opening is preparing.** When `open` returns, every path is ready: logged
   in, with the server running. Once the transfer starts, the transport never
   needs the user.
10. **Connections outside and paths inside are independent.** A component
    may open as many connections as it needs; each is an ordinary
    connection, whose only guarantee is order and wholeness. How many paths
    the transport opens below them, and how it spreads their messages over
    those paths, is not limited in any way but by speed. Connections to the
    same host may share the transport's paths there, so that a connection
    opened later needs no login of its own: the transport may carry several
    connections over one SSH connection, or one connection over several.

**Decided.** No message goes first. Errors and warnings are messages like
any other, and arrive in the order they were sent. Of the control messages,
only an error ever gained from going first: the handshake happens before any
data, the Destination's `Done` must follow its results anyway, and warnings
come as the transfer ends. An error that waits behind the data already
queued, a few MiB, arrives a fraction of a second later, and changes
nothing: the side that sent it stops either way. Without a message that goes
first, the transport has nothing to read in what it carries.

**Decided.** A **control connection** is for preparing, for the server's
life, and, while the transfer goes, for following it. It is apart from the
connections, and plain.

- **A byte stream to one program.** A control connection joins the
  component to one program on the host, its standard input and output, as
  bytes, much like a pipe. The program at the far end may be one that knows
  nothing of Ferry, such as `sh` before Ferry is installed, so the
  connection carries bytes, not messages; where messages are wanted, as for
  the handshake, the component frames them on the bytes itself. Reading
  comes to an end when the program ends; if it ends in failure, with a
  status other than 0, reading fails, as principle 7 asks.
- **What it is for.** While preparing, the client opens control connections
  in turn, each to the program it needs: a script that probes the host, an
  install, the fixtures of `ferry test`, and last the server, over whose
  control connection the handshake goes, the builds compared, the roles, the
  options, and the token by which the paths join it is learned. All of this
  comes before any connection is opened. While the transfer goes, the
  server's control connection is how the client follows it: the server
  sends its [metrics](profiling.md#following-a-transfer-as-it-goes) there
  every second, and answers there when asked where its threads are. Apart
  from the paths, none of it waits behind the transfer's data.
- **The server lives as long as its control connection.** When it closes,
  the server ends.
- **The server is bound to the first SSH connection.** Its control
  connection runs over the first SSH connection, so losing that one ends the
  server. A server that outlived its control connection, ending by its paths
  instead, was considered and not taken: a host whose systemd-logind has
  `KillUserProcesses=yes` ends a session's processes with the session
  anyway, and alone it would not save the transfer; see the next point.
- **Any SSH connection that drops fails the transfer,** the first or any
  other. The pieces on its path that were written but did not arrive are
  lost, so the far side finds one missing, and principle 7 fails the
  connection. The heartbeats find a drop that nothing else would, and a
  path that ends without its end marker fails the connection too; see
  [Connections](#connections). A rerun skips the files already complete.
  Paths that
  acknowledged their pieces, so that a sender could resend over the other
  paths what a dropped one lost, were considered and not taken, to keep the
  transport simple.
- **It is plain.** Its bytes go as they are, neither cut nor spread over
  paths: the principles of speed are not for it, since it carries little:
  before the transfer, and during it, a few hundred bytes a second.
- **Errors in the transfer do not use it.** Errors and warnings during the
  transfer go over the connection, in order with the rest.
- **Where it runs is the transport's.** Like everything below, which SSH
  connection carries it is hidden. It may share one with paths, so it needs
  no login of its own and is not counted by `--ssh-connections`.
- **A connection names its program too.** Opening a connection, the
  component names the command each path runs on the host, such as
  `ferry server --join <token>`; the transport runs it over every path, and
  the server's end puts the paths back together.


**Decided.** There is more than one transport, and the client does not know
which one it has. For each location, it asks for the transport of that
location, and uses only the two kinds of connection.

- **The SSH transport** reaches a host, as this section and
  [Connections](#connections) describe.
- **The local transport** reaches this machine. A control connection starts
  its program as a child process, joined by pipes. A connection's paths are
  local processes too, running the command named, such as
  `ferry server --join <token>`, and joining the server through its Unix
  socket. There is nothing to log in to, probe, or install: the server is
  the client's own binary.
- **Another way to reach a host** is another transport. Nothing above the
  transport changes for it.

**Proposed.** The transport chooses how many channels each connection
carries. A channel of an SSH connection can have only so much data
unacknowledged, its window, 2 MiB in OpenSSH (`CHAN_SES_WINDOW_DEFAULT`),
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

![Connections to a host: the ferry client runs one ssh master per SSH connection, each with its own TCP connection; over the first run the control connection, which starts the ferry server, and a path; over the others, a path each; every path is a ferry server --join process that hands its connection to the server through a Unix socket](../assets/architecture-connections.svg)

**Decided.** This is how the [transport](#transport) uses SSH. One `ssh`
master per SSH connection, and one TCP connection per master. Over them run
the [control connections](#transport), one of which runs the server, and
the paths, all equal, which join it. Every SSH connection is logged in
before the server starts.

- **One SSH connection by default, sharing your own ControlMaster if you
  have one.** The client reads the options that apply to the host from
  `ssh -G <host>`. If they set up a ControlMaster (`controlmaster` is not
  `false` and `controlpath` is not `none`), it adds no control options, and
  the first SSH connection shares your master. Otherwise it uses one of its
  own: `ControlMaster=auto`, a `ControlPath` under `/tmp/ferry-<uid>/ssh/`,
  and `ControlPersist=600`. Either way a login or OTP is typed once per
  host, and a later run within the persist time needs none. The control
  sockets live under `/tmp` because a Unix socket path is limited to 103
  bytes on macOS. No setting turns this off: a ControlMaster of your own is
  found without one.
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
  connection is only logging in; the control connections and the paths are
  sessions opened later over masters already logged in, with no login of
  their own. Holding the master also keeps it alive when it is your own
  ControlMaster without `ControlPersist`, which would otherwise end with the
  first `ssh` that used it.
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
- **Every path joins the server alike.** Once the server is running, the
  client opens the connection naming `ferry server --join <token>`, and the
  transport runs it over each of its paths, on every SSH connection, the
  first included. Each hands its standard input and output to the server
  through a Unix socket in a private directory on the host, and the token,
  random and learned over the server's control connection, says which
  transfer it joins. No port is opened.
- **Messages are spread over the paths and put back in order.** The
  transport cuts messages into numbered pieces, gives each piece to the path
  with the shortest queue, and puts the pieces back together, in order, on
  the far side, whichever path they took.
- **A path ends with an end marker.** Closing a connection, the sender
  writes, after the last piece on each path, a marker that it ends there,
  eight bytes. To the receiver, a path closed and a path cut short, as when
  its SSH connection drops, both just end; and one cut between two pieces
  would show nothing wrong while a piece it lost is waited for. A path that
  ends without its marker fails the connection.
- **Each SSH connection carries a heartbeat channel beside its path.** It
  joins the server as a path does, saying it is for heartbeats. Both sides
  send a heartbeat on every heartbeat channel every 5 seconds. A heartbeat
  channel on which nothing comes for 30 seconds, or which ends while the
  connection is open, fails the connection, and says which SSH connection,
  such as `ferry: the Destination on fatcat: SSH connection 2 of 4: no
  heartbeat for 30 s`, or `...: dropped`. A server waits 40 seconds instead,
  so that the client, which tells you, notices first; a server's own
  silence is for when the client is gone, and stops it, as its control
  connection closing does. The times are fixed. After a failure, the client
  gives each server a few seconds to end, then lets it go: an SSH
  connection that is gone never says that its sessions ended. Without heartbeats, an SSH connection that drops
  without a word, when the network goes away or a laptop sleeps, would
  leave both sides waiting, for hours, until TCP noticed. The heartbeats
  have a channel of their own so that they never wait behind data: a path's
  queue, or its SSH window when the far side reads slowly, holds back no
  heartbeat, and silence means the SSH connection is gone, not that the
  data is waiting. They still share the SSH connection's TCP connection,
  which is what they watch. The first SSH connection has one too, rather
  than using the control connection, which stays plain. The local transport
  has them as well, so that both transports run the same code.

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
  instead of running the wrong one silently.
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
  distribution's C library.
- **A dev build installs from source,** as in File Peeker. A dev build has no
  release to download, so the client uploads the source it was compiled from,
  and the host builds it. The build is identified by a hash of that source.
  The host needs a Rust toolchain.
- **The client asks before the first install on a host.** Later installs of a
  newer version on the same host do not ask. Without a terminal, the client
  installs only when given `--yes`.
- **Ferry never touches the host's `PATH`.** It runs the build it installed,
  even if another `ferry` is on the `PATH`.
- **Old builds are removed.** A host keeps the three most recently used
  builds. Each use of a build, and each install, marks its directory used, by
  its modification time, and removes the others beyond three.
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
- **CI also tests on Alpine,** where musl is the system's C library, not only
  linked into a static build.

## Packets on one connection

**Decided.** A connection is one bidirectional byte stream, carrying a
sequence of typed **packets**: `[type][length][payload]`. Every packet, of
whatever type, is a piece the [transport](#transport) sends over whichever
path it chooses. Each kind of message has its own type:

| Type | Direction | Content |
|---|---|---|
| control | both | handshake (versions, capabilities, options), errors, warnings |
| report | server → client | progress counts, how full the pipeline's queues are, paths a dry run lists, warnings, what could not be read or copied, for the client to show |
| manifest | Source → Destination | the Source's files: paths and metadata |
| needs | Destination → Source | the files the Destination needs |
| data | Source → Destination | file records |
| results | Destination → Source | each file's success or failure |

Packets of different types are interleaved, so that kinds of message that
flow at the same time do not wait for each other. While data is flowing, the
Source keeps sending manifest packets as its scan finds files, and the
Destination keeps sending needs and results. File data is cut into data
packets of at most 64 KiB, each carrying a piece of a compressed frame. One
pump thread per peer owns the connection's writer and takes packets from two
queues: messages first (manifest, needs, results, reports, and control
messages: an error, and the Destination's `Done`, which must follow the
results), then data. Control packets have no queue of their own and do not
go first; see [Transport](#transport). The data queue is bounded, so a Source faster than the
connection waits instead of piling the stream up in memory. The client,
between the servers, reads each packet's type only: reports it shows, and
everything else it passes on as it is.
