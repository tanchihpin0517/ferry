# Ferry architecture

Status: **draft, under discussion.** Each section says whether it is decided or
proposed. The goals and the rules for integrity and metadata are in the
[design overview](overview.md). How data moves through a transfer is in the
[pipeline](pipeline.md).

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
  the length of a transfer, where [extra connections](#ssh) join it.

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
- **One connection by default, sharing your own ControlMaster if you have
  one.** The client reads the options that apply to the host from
  `ssh -G <host>`. If they set up a ControlMaster (`controlmaster` is not
  `false` and `controlpath` is not `none`), it adds no control options, and
  the connection shares your master. Otherwise it uses one of its own:
  `ControlMaster=auto`, a `ControlPath` under `/tmp/ferry-<uid>/ssh/`, and
  `ControlPersist=600`. Either way a login or OTP is typed once per host, and
  a later run within the persist time needs none. The control sockets live
  under `/tmp` because a Unix socket path is limited to 103 bytes on macOS.
  No setting turns this off: a ControlMaster of your own is found without
  one.
- **`--connections N` opens more connections, each its own TCP connection.**
  A master carries all its sessions over one TCP connection, so more sessions
  on it are no faster. On a lossy path a single TCP flow is held well below
  the link's capacity by congestion control; to one host with about 0.01%
  loss, one connection reached about 31 MB/s and four together about 60 MB/s,
  the link's limit. The first connection is the default one above and carries
  every type of packet; the others carry only data, in whichever direction it
  flows. Each extra connection has a master of Ferry's own,
  `/tmp/ferry-<uid>/ssh/%C-1`, `%C-2`, and so on, kept for the persist time
  like the first.
- **Extra connections log in one at a time, before the transfer starts.**
  Each first tries `BatchMode=yes`, which succeeds with key authentication or
  a master still alive. If that fails, the client says which one is logging
  in, such as `logging in connection 2 of 4 to host`, and lets `ssh` ask in
  the terminal, one connection after another, so that prompts never
  interleave and the progress line never cuts into them. A host that accepts
  a TOTP code only once makes each login wait for the next code. Without a
  terminal, extra connections that need a login are skipped with a warning.
  A failed login, or Ctrl-C during one, stops opening more, and the transfer
  runs on the connections already open.
- **How an extra connection joins the transfer.** The client runs
  `ferry server --join <token>` over each extra connection. It hands its
  standard input and output to the transfer's server through a Unix socket in
  a private directory on the host, and the token, random and sent over the
  first connection, says which transfer it joins. No port is opened.
- **Data frames are spread over the connections and put back in order.** Each
  frame carries a sequence number. The sender gives each frame whole to the
  connection with the shortest queue, and the receiver's assembler takes
  frames in sequence, whichever connection they came over.

**Proposed.**

- **Without a terminal, a drop fails the transfer** instead of waiting for a
  login nobody can type, as in a script or cron job. A rerun skips the files
  already complete.
- **The client installs `ferry` on a host that lacks the version it needs;**
  see [Installing on a remote host](#installing-on-a-remote-host).

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

**Decided.** Peers talk over one bidirectional byte stream, as a sequence of
typed **packets**: `[type][length][payload]`. Each kind of message has its own
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
packets of at most 64 KiB, each carrying a piece of a compressed frame, and
control packets go first. A control packet therefore waits for at most one
64 KiB packet, never for a whole file or frame. One pump thread per peer owns
the connection's writer and takes packets from three queues: control, then
messages (manifest, needs, results, and the Destination's `Done`, which must
follow the results), then data. The data queue is bounded, so a Source faster
than the connection waits instead of piling the stream up in memory. A relay forwards packets as
they are; it reads control packets only to show progress.
