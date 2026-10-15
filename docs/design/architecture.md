# Ferry architecture

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md).

**Contents**

- [Client and servers](#client-and-servers)
- [Two peers, Source and Destination](#two-peers-source-and-destination)
- [Remote to remote goes through the client](#remote-to-remote-goes-through-the-client)
- [SSH](#ssh)
- [Transport](#transport)
- [Connections](#connections)
- [Installing on a remote host](#installing-on-a-remote-host)
- [Packets on one connection](#packets-on-one-connection)

![Peers in a push: the ferry client with the Source peer, and over SSH the remote ferry server with the Destination peer](../assets/architecture-peers.svg)

## Client and servers

**Decided.**

- **The client** is the `ferry` command you run in your terminal. It parses
  arguments, holds the SSH connections, runs the local side of the transfer,
  and shows progress, warnings, and the final summary. It is not a background
  process: it lives as long as the command does.
- **A server** is a `ferry server` process that the client starts on a remote
  machine over SSH, as rsync starts its remote side. It speaks the protocol on
  its standard input and output and exits when the SSH connection closes. It
  is not long-lived, listens on no port, and keeps no state between
  connections. The only socket it listens on is a private Unix socket, for
  the length of a transfer, where its [paths](#connections) join it. It
  lives as long as its [control connection](#transport).

A transfer runs only while its client runs, because the client is one end of
it or, from remote to remote, the relay between both ends. When the client
exits or the connection drops, the server exits too. Files already renamed
into place are complete, so the next run skips them; a file that was not
finished is sent again from the start.

## Two peers, Source and Destination

**Decided.** A transfer has two **peers**, one for each side. Both run the same
code, whether in the client or in a server, and the handshake decides which
one is the **Source** and which one is the **Destination**. Push and pull are
both supported:

| Command | Source | Destination |
|---|---|---|
| `ferry copy ./a host:b` (push) | client | server on host |
| `ferry copy host:b ./a` (pull) | server on host | client |
| `ferry copy ./a ./b` (local copy) | client | client |
| `ferry copy hostA:a hostB:b` (remote to remote) | server on hostA | server on hostB |

A local copy runs both peers in the client, joined by an in-memory pipe, over
the same protocol, and needs no server. Only one code path has to be
maintained.

**Decided.** The Destination decides what to send. The comparison runs on the
Destination, as in rsync; see [pipeline section 2](pipeline.md#2-compare).

## Remote to remote goes through the client

**Decided.**

![Remote to remote: the ferry client relays packets between host A's Source peer and host B's Destination peer, over SSH to each](../assets/architecture-relay.svg)

The client connects to both servers and forwards packets between them without
decoding the data. The two remote machines do not need to reach each other,
and no credentials are placed on either of them. The cost is that the data
crosses the local machine's network link twice.

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

![The transport: the peers see one connection, where one sends messages and the other receives them whole and in order; underneath, the sending side's transport cuts messages into numbered pieces and gives each to the path with the shortest queue, every path equal, and the receiving side's transport puts them back together in order](../assets/architecture-transport.svg)

**Decided.** Every byte between the peers goes through one module, the
**transport**. Its principles:

1. **Two kinds of connection to the outside.** Through the transport, a
   component opens a **connection**, then sends messages with `send` and
   receives them with `recv`; the principles below are about these. Or it
   opens a host's **control connection**, for preparing; see below. Neither
   shows anything of SSH.
2. **Messages arrive whole and in order.** No message is lost, added,
   repeated, or corrupted, and messages arrive in the order they were sent.
3. **Its one goal is speed.** Within that guarantee, the transport moves the
   data as fast as it can.
4. **Every path below is equal.** The SSH connections and channels the
   transport uses are all alike, with no role of their own: none is for
   control, none for data. How many there are, and which one each piece
   takes, is the transport's choice; the outside neither sees nor chooses
   it.
5. **Everything else happens inside.** Cutting messages into pieces, sending
   the pieces over the paths, and putting them back together and in order on
   the far side are the transport's work.
6. **It does not read what it carries.** Every message is the same to the
   transport; it cannot tell an error from data.
7. **It fails rather than deliver anything wrong.** If the guarantee cannot
   be kept, such as when a path drops and cannot be restored, `send` and
   `recv` fail. A message is never delivered in part or out of order.
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
nothing: the side that sent it stops either way. Without a first lane, the
transport has nothing to read in what it carries.

**Decided.** A host's **control connection** is for preparing, and for the
server's life. It is apart from the connections, and plain.

- **What it is for.** The client opens it first, while preparing, and uses
  it to run commands on the host, such as probing, installing, and the
  fixtures of `ferry test`; to start the server and learn the token by
  which the paths join it; and for the handshake: the builds compared, the
  roles, the options. All of this comes before the connections are used.
- **The server lives as long as it.** When the control connection closes,
  the server ends. A path that drops is only one path fewer.
- **It is plain.** Its messages go as they are, neither cut nor spread over
  paths: the principles of speed are not for it, since it carries little,
  and before the transfer.
- **Errors in the transfer do not use it.** Errors and warnings during the
  transfer go over the connection, in order with the rest.
- **Where it runs is the transport's.** Like everything below, which SSH
  connection carries it is hidden. It may share one with paths, so it needs
  no login of its own and is not counted by `--ssh-connections`.
- **A local copy has none.** With no host, there is nothing to prepare or
  to start.

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

![Connections to a host: the ferry client runs one ssh master per SSH connection, each with its own TCP connection; over the first run the control connection, which starts the ferry server, and a path; over the others, a path each; every path is a ferry server --join process that hands its connection to the server through a Unix socket(../assets/architecture-connections.svg)

**Decided.** This is how the [transport](#transport) uses SSH. One `ssh`
master per SSH connection, and one TCP connection per master. Over them run
the host's [control connection](#transport), which starts the server, and
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
  which carries the control connection, is the transport's choice. The name
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
  connection is only logging in; the control connection and the paths are
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
  transport runs `ferry server --join <token>` over each of its paths, on
  every SSH connection, the first included. Each hands its standard input
  and output to the server through a Unix socket in a private directory on
  the host, and the token, random and learned over the control connection,
  says which transfer it joins. No port is opened.
- **Messages are spread over the paths and put back in order.** The
  transport cuts messages into numbered pieces, gives each piece to the path
  with the shortest queue, and puts the pieces back together, in order, on
  the far side, whichever path they took.

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

**Decided.**

- **A client and a server run exactly the same build.** The protocol needs no
  compatibility with older versions, and can change freely between them. The
  handshake still compares builds, and refuses a mismatch with a clear error
  instead of running the wrong one silently.
- **Builds are installed side by side.** Each build has a directory of its own
  on the host, and the client runs the one matching itself. Two clients of
  different versions, say on a laptop and a desktop, can reach the same host
  without overwriting each other. Since servers are not long-lived, there is
  never a running server to replace.
- **A release build installs a binary.** The client downloads the release of
  its own version for the host's OS and architecture from GitHub Releases,
  checks it against `SHA256SUMS`, caches it locally, and uploads it over the
  SSH connection. The host needs no outbound network access, which cluster
  compute nodes often lack. Linux binaries are static musl builds, so they run
  on any distribution's C library. musl cannot see LDAP or SSSD accounts, so
  user and group names are looked up through `getent`; see the
  [overview](overview.md#names-are-looked-up-with-getent-on-linux).
- **A dev build installs from source,** as in File Peeker. A dev build has no
  release to download, so the client uploads the source it was compiled from,
  and the host builds it. The build is identified by a hash of that source.
  The host needs a Rust toolchain.
- **The client asks before the first install on a host.** Later installs of a
  newer version on the same host do not ask. Without a terminal, the client
  installs only when given `--yes`.
- **Ferry never touches the host's `PATH`.** It runs the build it installed,
  even if another `ferry` is on the `PATH`.

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
- **Old builds are removed.** The client keeps the few most recently used
  builds on each host and removes the rest.
- **`--remote-ferry <path>`** runs a given binary instead, like rsync's
  `--rsync-path`. It is for hosts where the home directory is not writable or
  has no room.
- **Releases are built by CI.** A release workflow builds the binaries for
  Linux and macOS on x86_64 and aarch64 and publishes them with `SHA256SUMS`,
  as File Peeker's does.

## Packets on one connection

**Decided.** A connection is one bidirectional byte stream, carrying a
sequence of typed **packets**: `[type][length][payload]`. This is how the
[transport](#transport) carries its lanes over the first connection; extra
connections carry only data packets. Each kind of message has its own
type:

| Type | Direction | Content |
|---|---|---|
| control | both | handshake (versions, capabilities, options), errors, warnings, progress, summary |
| manifest | Source → Destination | the Source's files: paths and metadata |
| needs | Destination → Source | the files the Destination needs |
| data | Source → Destination | file records |
| results | Destination → Source | each file's success or failure |

Packets of different types are interleaved, so that kinds of message that
flow at the same time do not wait for each other. While data is flowing, the
Source keeps sending manifest packets as its walk finds files, and the
Destination keeps sending needs and results. File data is cut into data
packets of at most 64 KiB, each carrying a piece of a compressed frame. One
pump thread per peer owns the connection's writer and takes packets from two
queues: messages first (manifest, needs, results, and control messages:
errors, warnings, and the Destination's `Done`, which must follow the
results), then data. Control packets have no queue of their own and do not
go first; see [Transport](#transport). As built, until the transport is,
they still have one. The data queue is bounded, so a Source faster than the
connection waits instead of piling the stream up in memory. A
relay forwards packets as they are; it reads control packets only to show
progress.
