# Ferry protocol

[中文](protocol.zh-TW.md)

Every connection a transfer uses: between whom, how it is made, what it
carries, and how it ends, to the byte. Why it is so is in the
[architecture](architecture.md), its [Transport](architecture.md#transport),
[Connections](architecture.md#connections) and
[Monitors](architecture.md#monitors); what the peers do with what they read
is in the [pipeline](pipeline.md). Both ends of every connection Ferry
speaks on are the same build, as the handshake checks, so nothing here is
versioned: but for the first fields of `Hello` and of `Refused`, which a
client of another build reads, any of it may change from one build to the
next.

**Contents**

- [Every connection](#every-connection)
- [Two kinds of connection](#two-kinds-of-connection)
- [What goes as it is](#what-goes-as-it-is)
- [Each connection, in order](#each-connection-in-order)
  - [The SSH connection](#the-ssh-connection)
  - [The first session](#the-first-session)
  - [The link](#the-link)
  - [Sudo's check](#sudos-check)
  - [A control connection](#a-control-connection)
  - [A lifeline](#a-lifeline)
  - [The streams](#the-streams)
  - [A handover](#a-handover)
  - [A fixture](#a-fixture)
  - [Releases, and the rest](#releases-and-the-rest)
  - [`--remote-join`](#--remote-join)
- [How a connection carries pipes](#how-a-connection-carries-pipes)
  - [Layers](#layers)
  - [A connection's states](#a-connections-states)
  - [What preparing carries](#what-preparing-carries)
  - [The pipes of each connection](#the-pipes-of-each-connection)
  - [Frames](#frames)
  - [Compression](#compression)
  - [Windows](#windows)
  - [Ends and failures](#ends-and-failures)
- [Messages](#messages)
- [What the pipes carry](#what-the-pipes-carry)

## Every connection

![Every connection of a transfer, the client and the Source's server on this machine, a host's monitor and the Destination's server on the host; solid lines carry pipes, dashed ones bytes as they are, numbered in the order they are made. 1, the one SSH connection, TCP, whose channels are every connection across the machines. 2, the first session, through the login shell, as it is: probing, installing, and becoming the host's monitor; to install, the client fetches SHA256SUMS, and the binary to upload, from GitHub by curl, and the host downloads the release by curl or wget. 3, the link, between the client's monitor and the host's, one control pipe. 4, sudo's check, for --remote-sudo alone, as it is; a fixture, for ferry test alone, as it is. 5, a control connection, between the client and each server, the reports and control, not compressed on this machine; a server's standard error is its lifeline, to its monitor, as it is, never across the network. 6, the streams, between the two servers, the manifest, the data, the answers and control; the end on this machine handed to its server through the server's socket.](../assets/protocol-connections.svg)

**Decided.** Every connection a transfer uses, the Destination on a host
and the client and the Source on this machine, in the order they are made,
numbered as in the figure: a lifeline with the control connection of the
server it is made for, a handover with the stream it hands over:

| # | Connection | Between | Carries | How |
|---|---|---|---|---|
| 1 | [the SSH connection](#the-ssh-connection) | the client's `ssh` master and the host's `sshd` | SSH's own protocol; every connection across the machines is one of its channels | not Ferry's |
| 2 | [the first session](#the-first-session) | the client and the host's `sh`, which becomes the monitor | marked lines; once it is the monitor, nothing, held open | as it is |
| 3 | [the link](#the-link) | the client's monitor and the host's | its control pipe | pipes |
| 4 | [sudo's check](#sudos-check), for `--remote-sudo` | the client and `sudo ferry build-id`, which the monitor starts | the password line; one line back, the build | as it is |
| 5 | [a server's control connection](#a-control-connection) | the client and the server, which the monitor starts, or the client on this machine | control and the reports | pipes, not compressed on this machine |
| 5 | [a lifeline](#a-lifeline) | each program a monitor starts, by its standard error, and that monitor | text for people; a marked line a second, how long since its stream heard the far side | as it is |
| 6 | [the streams](#the-streams), 1 to 8 | the Source's server and the Destination's | control, the manifest, the data, the answers | pipes |
| 6 | [a handover](#a-handover) | the client, or a joining process, and a server's or the monitor's socket | one byte and two descriptors | as it is |
| — | [a fixture](#a-fixture), for `ferry test` | the client and `ferry fixture` | its output, read to its end | as it is |
| — | [a release](#releases-and-the-rest), to install | the client, or the host's first session, and GitHub | HTTPS, by `curl` or `wget` | not Ferry's |

In turn:

1. The client opens the host's SSH connection: it logs in, and runs
   nothing; a password or an OTP is asked for in the terminal.
2. The first session probes the host, installs this build there if it
   must, downloaded by the host or uploaded by the client, and last becomes
   the monitor, which says its build, its socket, its port and its secret.
3. The client's monitor follows the host's on the link, and the heartbeat
   starts. From then on, every program on the host is started by the
   monitor, each on a channel of its own, each with its standard error as
   its lifeline.
4. With `--remote-sudo`, sudo's check: whether it asks for a password, and
   whether the one given is right.
5. A control connection to each server: after the handshake both wait,
   while the client checks the two roots against each other, then tells
   them to go on, and how many streams come.
6. The K streams; the end of each on this machine is handed to its server.
7. Once the transfer is done, the control connections and the streams end;
   the link closes once everything on the host has ended, the first session
   last.

With both sides on this machine, there are none of 1 to 4: the client
starts both servers through pipes, their lifelines are its own, and both
ends of each stream are handed over.

## Two kinds of connection

- **Those that carry pipes:** the link, a control connection, a stream.
  Both ends are Ferry's. Each is first **prepared**, in bytes as they are:
  it reaches its program, says what it is for, is handed over, and its
  settings are told; once it is in the hands of the programs that use it,
  it is **ready**, and becomes the pipes of its kind. From then on the
  programs read and write only on pipes, and the transport frames,
  windows and compresses what they write; see
  [How a connection carries pipes](#how-a-connection-carries-pipes).
- **Those that go as they are,** from first to last: the first session, a
  lifeline, sudo's check, a fixture, a handover. The far end is not Ferry,
  or anyone may write on it, or it passes descriptors alone. The SSH
  connection and GitHub are of this kind too, but are not Ferry's
  protocol.

## What goes as it is

**Decided.** What goes as it is, before a connection is ready, or on one
that never is:

| What | Bytes | Who writes it, who reads it |
|---|---|---|
| straight | one byte, 1 | the client; the socket's listener, which answers with the start mark |
| a port's secret | 64 hexadecimal digits | the client; the port's listener, which lets go of a connection that gives another |
| the start mark | `\0ferry: start\0`, 14 bytes | the far program, after which the program that uses the connection writes; the reader skips to it |
| the end mark | `\0ferry: end\0`, 12 bytes | a server, after its answer to the handshake; the client, which finds by it an answer cut into |
| a marked line | `\0ferry:<tag>:<kind>:<length>:<content>\n` | the host on its first session, the tag the client's, 16 hexadecimal digits new each run; a lifeline's, its tag `lifeline` |
| a packet | `[type][length][payload]`, the type a byte, the length four, little-endian, the payload postcard | the client's `Follow`, `Launch` and `Hello`; a server's answer |
| the password line | the password, and a newline | the client; `sudo -S`, for `--remote-sudo` where it asks |
| a stream's header | `[number][level]`, two bytes, the level 0 for none | the client; the server |
| a handover | one byte, 0, with two descriptors by `SCM_RIGHTS`; one byte back | the client, or a joining process; the server, or the monitor, which says so that it took them |

A connection to a socket or a port on which nothing comes first, neither
its first byte nor its first packet, is let go after 30 seconds; of those
to a port, at most 16 are waited on for their secret at once.

## Each connection, in order

In the order a transfer makes them. In the sequence figures, an arrow ends
at the program that reads what it carries; a green arrow is the start mark.
A green bar below is ready, pipes from then on; a grey one, as it is from
first to last.

### The SSH connection

Not Ferry's protocol.

- One TCP connection to each host, through `ssh`'s ControlMaster: your
  own, if you set one up, or else one of Ferry's, which an `ssh -N` holds.
  A password or an OTP is asked for in the terminal as it logs in.
- `ssh -O check` asks the master, through its control socket on this
  machine, whether it is still there.
- Every connection across the machines is a channel of it; should it drop,
  the transfer fails, and nothing connects again.

See [Connections](architecture.md#connections) for its options.

### The first session

![The first session, as it is: the client and the host's sh, which becomes the monitor. ssh runs sh -c <the probe's script>, through the login shell; marked lines: probe … done, what the shell prints perhaps among them; curl: SHA256SUMS; the download's script, its URL and SHA-256; curl or wget: the release; marked lines: install … done; or, where the host cannot download: the upload's script, then the binary or the tar as it is; ferry_become <ferry> monitor --tag <tag>; exec, the monitor; marked lines: the monitor's build, socket, port and secret, done. After that nothing goes: the client holds it, and letting go is leaving. Each marked line is \0ferry:<tag>:<kind>:<length>:<content>\n, in one write; the client reads only lines of its own tag. Until the monitor, the client sends a : every 5 seconds, and the host prints an alive line every 5 seconds. As it is, no pipes: the far end is sh at first, and what the shell left running may write at any time.](../assets/protocol-flow-first-session.svg)

- **Its ends:** the client's `ssh`, and the host's login shell, which runs
  `sh -c '<script>'`: the host's one session. The script is one line of
  `printf` with octal escapes, which any login shell hands to `sh` as it
  is (`src/remote/shell.rs`).
- **Host to client: marked lines,** each in one write. The length is the
  content's bytes, so that a line printed into is found. A part ends with a
  `done` line, its content the part's count of lines, so that one lost
  shows. The client reads only the lines of its own tag, takes the rest
  for what the shell printed, and says so once.
  - `probe`: what the probe found, a `key=value` a line: the machine ID,
    the platform, Ferry's directory on the host, which builds are there,
    whether there is cargo, curl or wget, and a way to take SHA-256, and
    what each root is.
  - `install`: what installing did.
  - `alive`: until the monitor, a line every 5 seconds, counted in no
    part.
  - `monitor`: the monitor's `build=`, `socket=`, `port=` and `secret=`,
    then `done`. The client compares the build here, and refuses another.
- **Client to host: a command a line.** The script reads a line and runs
  it: the install's script, and last
  `ferry_become <ferry> monitor --tag <tag>`. With nothing else to send,
  the client sends a `:` every 5 seconds; the host's watchdog ends the
  session once it has heard nothing from the client for 30 seconds, a file
  being uploaded growing counting as word from it.
- **Uploading.** Where the host cannot download the release, or check its
  SHA-256, the client sends the upload's script, then the binary as it is,
  or, for a development build, a tar of its source, which the host's `cat`
  reads to its end. It is checked before it is installed: a binary by its
  SHA-256, by its size where the host has no tool for it; a development
  build, once built, must run and say it is that build. The upload is the
  last thing the session reads: the script, done reading, becomes the
  monitor.
- **Standard error** goes straight to the client's terminal, the monitor's
  too, and the servers' it hands on.
- **Its end.** Once it is the monitor, nothing more goes; the client holds
  it, and once the client lets go, the monitor reads its input's end, and
  takes the client for gone. It ends only after the link has closed.
- **Why as it is:** the far end is `sh` at first, and the shell's startup
  files, and what they leave running, share its standard output, and may
  write there at any time.

### The link

![The link, carrying pipes: the client's monitor and the host's. 01, or the port's secret; the start mark: it got there; Follow { compression }: what the connection is for, and the link's settings. Ready: the link's control pipe, both ways. The listener answers 01 with the start mark; one from the port must first give the right 64 hexadecimal digits. With no start mark, the client tries the port.](../assets/protocol-flow-link.svg)

1. Client to listener: `01`, or the port's secret.
2. Listener to client: the start mark. Should none come, SSH forwards to
   no socket, and the client tries the port.
3. Client to monitor: `Follow { compress }`, which says the connection is
   the link: until the monitor reads it, it does not know whether it keeps
   the connection.
4. Both ends ready: one control pipe, both ways.

The client also times the round trip on it, by `Ping` and `Pong`, which
says how many streams the transfer has.

### Sudo's check

![Sudo's check, as it is: for --remote-sudo alone, once a host. 01 or the secret; the start mark; Launch { id, SudoCheck { Nothing } }; the connection handed over: sudo -n -- ferry build-id starts; one line, the build, read to its end; on the link: Ended { id, exit code, standard error }. Where that fails: the client asks for the password, opens another, Launch { SudoCheck { Password } } and the password line, and sudo -k -S -p "" -- ferry build-id starts. Asked at most 3 times. A fixture goes the same way, for ferry test alone: Launch { Fixture }, ferry fixture --until-stdin-closes starts, its output read as it is. As it is, no pipes: the far end is sudo, run once.](../assets/protocol-flow-sudo.svg)

- For `--remote-sudo` alone, once a host, before any server starts.
- The client opens a channel to the monitor's socket, whose first two steps
  are the link's, then sends `Launch { id, SudoCheck { Nothing } }`: the
  monitor starts `sudo -n -- ferry build-id`, first with no password.
- Where that fails, the client asks for the password in the terminal,
  opens another, sends `Launch { id, SudoCheck { Password } }`, and writes
  the password line as it is: the monitor starts
  `sudo -k -S -p "" -- ferry build-id`. It asks at most 3 times.
- The far end prints one line, the build, which the client reads to its
  end; how it ended, and what it said on standard error, the monitor says
  on the link, in `Ended`.
- Each server is then started through sudo, its password line sent before
  `Hello` in the same way.
- **Why as it is:** the far end is sudo, run once, its output read to its
  end.

### A control connection

![A control connection, carrying pipes: the client and a server on a host. 01 or the secret; the start mark; Launch { id, Server }; the connection handed over: ferry server starts, through sudo, its standard error its lifeline; the password line, where one is asked for; Hello { build, role, root, compression, … }; the start mark, Welcome { socket, port, the root's identity }, the end mark. Ready: the reports, server to client, and control, both ways. The client writes them in turn, waiting for nothing; each part is read by the program its arrow ends at, the next only after. On this machine there is no listener, monitor or sudo: the client starts the server through pipes, and there are only Hello and the answer.](../assets/protocol-flow-control.svg)

On a host:

1. Client to listener: `01` or the secret; listener to client: the start
   mark.
2. Client to monitor: `Launch { id, Server { sudo, port } }`. The monitor
   starts the server, through sudo with `--remote-sudo`, with the
   connection as its standard input and output, and reads nothing more on
   it; the server's standard error is its [lifeline](#a-lifeline).
3. Client to sudo: the password line, only where one is asked for.
4. Client to server: `Hello`; see [Messages](#messages).
5. Server to client: the start mark, then `Welcome`, or `Refused`, or
   `Error`, then the end mark, in one write.
6. The server is ready once it has written its answer, the client once it
   has read the end mark: the reports and control.

The client writes steps 2 to 4 in turn, waiting for none: each part is read
by the program it is for, and the next only after.

On this machine, the client starts `ferry server --lifeline` through pipes,
and there are steps 4 to 6 alone.

Once ready, the server waits, having changed nothing, until the client has
checked the two roots against each other and sends `Proceed`, with what the
server must never go into or delete and how many streams come; a server
whose control connection closes instead ends.

### A lifeline

![A lifeline, as it is: a worker's standard error and its monitor, never across the network. The worker starts: one end of a socket pair as its standard error; text for people: sudo's messages, a panic; \0ferry:lifeline:heard:<length>:<milliseconds>\n, a line a second; writes nothing; once its end closes, the worker wakes, and stops. As it is from first to last, until the worker ends or the monitor is gone. heard is how long since the worker's stream got anything from the far side: that came over the host's SSH connection from the client's side, and the monitor takes it for word from the client. Text for people: on a host, to the first session's standard error; for a worker run to its end, up to 64 KiB, sent back in Ended.](../assets/protocol-flow-lifeline.svg)

- **Its ends:** the standard error of each worker a monitor starts, a
  server, sudo's check, a fixture; and that monitor, the host's on a host,
  the client on this machine. They are the two ends of a socket pair,
  never across the network.
- **Worker to monitor:** standard error's text for people, sudo's
  messages, a panic; among it, a marked line a second,
  `\0ferry:lifeline:heard:<length>:<milliseconds>\n`, how long since the
  worker's stream got anything from the far side. That came over the
  host's SSH connection from the client's side, and the monitor takes it
  for word from the client. No one but the worker writes here, and text
  for people has no NUL, so the tag is fixed.
- **Monitor to worker:** nothing. The worker waits for it to be readable,
  which it is only once the monitor's end closes: however the monitor
  ends, killed or crashed, the worker sees it, and stops. No timer, and
  nothing over the network.
- **Where text for people goes:** a server's on a host, by the monitor to
  its own standard error, the first session's, to the client's terminal;
  a server's on this machine, by the client to the terminal. Of a worker
  run to its end, sudo's check or a fixture, the monitor keeps up to
  64 KiB, and says it on the link in `Ended`.
- **Where sudo logs its output,** a pipe of sudo's own comes between, and
  the worker, finding its standard error no socket, says its lifeline is
  not to be relied on.
- **Why as it is:** anyone may write on standard error, sudo does, a panic
  does; and it never crosses the network.

### The streams

![A stream, carrying pipes: between the two servers, made by the client. Written first: 01 or the secret (a host), the number, the level; written first: the same on the other end. Handed over: on this machine by a handover; on a host, joined to ssh -W. The start mark, written by the Destination's side; the start mark, written by the Source's side. Ready: the manifest and the data (S to D), the answers (D to S), control (both ways). A server on a host has its listener read 01 and answer with the start mark; where the other server is on this machine, that server writes nothing first, and the client writes it for it. A server is ready once it has read the number and the level, and skipped to the start mark from the far side. Made once for each of the K streams, numbered 0 to K−1.](../assets/protocol-flow-stream.svg)

1. The client makes a socket pair, and writes into each end first: for a
   server on a host, `01` or the secret; the header, `[number][level]`;
   and, where the other server is on this machine, a start mark, as a
   server on this machine writes nothing first.
2. The client hands both ends over: to a server on this machine by a
   [handover](#a-handover); to one on a host, as the standard input and
   output of `ssh -W` to its socket. Between two hosts, the two channels
   are so joined back to back, the kernel moving the bytes, the client
   handling none.
3. A server on a host has its listener read `01` or the secret, and
   answer with the start mark, which goes through the socket pair to the
   other server.
4. Each server reads the header, skips to the start mark from the far
   side, and is ready: the manifest, the data, the answers and control.

Each of the K streams is made so, numbered 0 to K−1, where
K = min(8, max(1, ⌈RTT ÷ 15 ms⌉)), the round trip to the farthest host as
the link times it, and one on this machine; `Proceed` tells each server
how many. The first carries every pipe; the others carry the data's chunks
alone.

### A handover

![A handover, as it is: a connection given to a server. connect; 00, with two descriptors (SCM_RIGHTS); 1 byte: taken. The client then lets go of its own; a joining process waits until the socket closes. A stream on this machine: one end of a socket pair, once to read, once to write; held by the client until taken, as macOS empties one that no one holds. A joining process (--remote-join): its own standard input and output. As it is, no pipes: only descriptors pass.](../assets/protocol-flow-handover.svg)

- **A stream on this machine:** the client connects to the server's socket
  on this machine, and sends one byte, `00`, with the descriptor of one
  end of a socket pair twice, once to read and once to write
  (`SCM_RIGHTS`); the server takes them, and answers with one byte. Until
  then the client holds that end: on macOS, a socket in flight to a
  connection not yet accepted, which no process holds, is emptied as
  garbage.
- **`--remote-join`:** the joining process connects to the server's, or
  the monitor's, socket, and sends `00` with its own standard input and
  output; it then waits until the far end closes the socket, as the host's
  session ends with it.
- The socket is in a directory only this user can open, named by a random
  token.
- **Why as it is:** only descriptors pass, no data.

### A fixture

- For `ferry test` alone: making, changing and checking trees for tests on
  a host. It goes as sudo's check does.
- The client opens a channel to the monitor's socket, and sends
  `Launch { id, Fixture { arguments } }`: the monitor starts
  `ferry fixture --until-stdin-closes <what>`, to make, change, take a
  digest of, create, mark, delete or check.
- What it prints, as a digest, comes back as it is, and the client reads
  it to its end. Its input only stops it: once the client closes it, or
  the connection breaks, it stops.
- How it ended, and what it said on standard error, `Ended` says, as for
  sudo's check.
- **Why as it is:** it runs once, its output read to its end, and only in
  tests.

### Releases, and the rest

- **GitHub releases,** not Ferry's protocol: the client fetches this
  version's `SHA256SUMS` by curl, and, to upload, that platform's binary;
  the host downloads the release by curl or wget in its first session, and
  checks it against `SHA256SUMS` before installing it.
- **A child's pipe:** a server runs `getent` to look up owners' names, and
  reads its output by a pipe. It is no connection, and is here only to be
  complete.
- **In tests alone:** a debug build's `ferry relay`, which joins standard
  input and output to a socket or a port in place of `ssh -W`, for tests
  with no `ssh`.

### `--remote-join`

For a host whose SSH forwards neither to a socket nor to a port, as the user
says with `--remote-join`. The link's, a control connection's, and the
streams' channels then run through the host's shell, as
`ferry server --join <socket>`: the joining process first writes the start
mark on its standard output, then hands its standard input and output to
the socket ([A handover](#a-handover)). Then as above, without `01`. What
the shell prints comes before the start mark, and the reader skips it;
what the shell left running writes after it only into the protocol, which
fails the transfer.

## How a connection carries pipes

Of the link, a control connection and a stream alone, once ready.

### Layers

![Layers: the writer and the reader, four layers each. The program, a peer, the client or the monitor, writes what it has, waits while it can write no more, not knowing how much one write takes; the reader reads what came, and takes its records out itself. The pipe: a set for each kind of connection, each pipe's use and number fixed, one way, in order, reliable, whole once its End is read. The transport: send buffers, compression, the pump's scheduling and cutting into frames, the windows kept; the receiving thread only sorts, then decompresses, giving back window as much as was read. The connection, the wire: bytes as they are while preparing, frames once ready, over an SSH channel, a socket pair or a pipe. Preparing's bytes do not take this path; nothing is compressed on this machine, every pipe across the network is.](../assets/protocol-layers.svg)

- **The program:** a peer, the client, a monitor. It writes with
  `write(&[u8])`, which returns once all is taken, waiting while it can
  write no more, not knowing how much one write takes; and reads what came,
  taking its records out itself. No program sees frames, compression or
  windows.
- **The pipe:** one for each kind of content, one way, in order, reliable.
  Its writer closes it once done, and the transport sends its `End`; its
  reader has all of it once it reads the `End`. Each kind of connection has
  its set of pipes, their uses and numbers fixed; see
  [The pipes of each connection](#the-pipes-of-each-connection). Pipes keep
  no order between them, and a writer does not know whether the far end
  read. A writer may `sync`, which returns once the far program has read
  all written before it; nothing in Ferry syncs yet.
- **The transport:** sending, each pipe's send buffer, its compression and
  the pump, which schedules, cuts into frames, and keeps the windows;
  receiving, a thread that only sorts frames into their pipes, and never
  waits for a program.
- **The connection,** the wire: an SSH channel, a socket pair or a pipe.
  Bytes as they are while preparing; frames alone once ready.

### A connection's states

![A connection's states, each end its own, only forward. Preparing: bytes as they are, reaching, saying what it is for, handing over, telling settings, with no pipes, windows or compression. Once both ends are the programs that use it and its settings are told, Wire::stream() makes it ready: pipes, windows and compression, the programs reading and writing only here, a frame its pipe, kind, length and content. Once each pipe has read its End, both ways, the connection ends. Broken while preparing, or once ready before an End: failed, and the transfer with it, nothing connecting again. Each end turns once.](../assets/protocol-states.svg)

- **Preparing.** The connection reaches the programs that use it, and the
  settings for ready are told, in bytes as they are, with no pipes, windows
  or compression, as each connection's steps above go.
- **Ready.** Both ends are the programs that use it, its settings told, and
  the transport makes it the pipes of its kind. Each end turns once, at
  `Wire::stream()` (`src/transport/wire.rs`).
- **Ended.** Once every pipe, both ways, has read its `End`; see
  [Ends and failures](#ends-and-failures).
- **Failed.** Broken while preparing, or once ready before its `End`s: the
  transfer fails, and nothing connects again. The one exception is
  preparing's first step: a channel straight to a socket on which no start
  mark comes means SSH forwards to no socket, and the client tries the
  port.
- Each end has its own state, and need not turn at once: on a control
  connection, the server is ready once it has written its answer, the
  client once it has read the end mark.

### What preparing carries

- **Each kind of connection's preparing carries its own pipes' settings:**
  the link's, `Follow`; a control connection's, `Hello`; a stream's, the
  header the client writes. Which pipes there are, and their numbers, is
  fixed by the kind of connection, as both ends are the same build, so
  none is declared.
- **The client decides the compression for each connection:**
  - `--no-compress`: none;
  - `--compress-level N`, N from 1 to 19: every connection at N, this
    machine's too;
  - otherwise: a connection to a host at zstd level 1, one on this machine
    not at all.

### The pipes of each connection

Each record on a pipe is its length, a varint, then its postcard encoding;
one is at most a segment and 64 KiB, now 2 MiB + 64 KiB, and a longer one
breaks the protocol, as does a pipe number or a kind not known. What each
pipe carries is in [What the pipes carry](#what-the-pipes-carry); the
messages on control and the reports are [packets](#messages).

**The link**

| Number | Pipe | Direction | Carries | Window |
|---|---|---|---|---|
| 0 | control | both | `Beat`, `Ping` and `Pong`, `Stop`, `Look` and `Looked`, `Ended`, `NotStarted`, `Paused`, `Resumed`, `Awaiting` | none |

**A control connection**

| Number | Pipe | Direction | Carries | Window |
|---|---|---|---|---|
| 0 | control | both | the client's `Proceed` and `Snapshot`; the server's metrics and what the stream carried, every second, its snapshot, its failure, and that its peer is done | none |
| 1 | reports | server → client | counts, the paths a dry run lists, warnings, queues, the files being transferred, what the threads are busy with, levels; last, the profile | 1 MiB |

**A stream**

| Number | Pipe | Direction | Carries | Window |
|---|---|---|---|---|
| 0 | control | both | an error | none |
| 1 | manifest | Source → Destination | `Root`, `In`, `Entry`, `Listed`, `Unlistable`, `User`, `Group`, `Problem`, `End` | 2 MiB |
| 2 | data | Source → Destination | `File`, `Segment`, `Hash`, `Unopened`, `End` | 64 MiB; its chunks on every stream |
| 3 | answers | Destination → Source | `File`, `Attributes`, `Compared`, `Failed`, `NotDeleted`, `NeedsEnd`, `Done` | 1 MiB |

### Frames

![Frames and compression once ready. a, a frame: its pipe a byte, its kind a byte, its length four bytes, then its content, at most 64 KiB; kind 0 Bytes, the pipe's next content; 1 Window, window given back; 2 End, the pipe written to its end, the data's with its count of chunks; 3 Sync, a sync, with the bytes written before it; 4 Synced, the answer, the far program has read so far. A pipe's number is fixed by the kind of connection. b, a pipe compressed as a stream, the manifest for one: records one after another, compressed as one zstd stream, cut into Bytes frames; records, zstd's blocks and frames each have bounds of their own. c, the data: each chunk its number, as it is or zstd, its length and content; a chunk may be cut into several frames, all on one stream, other pipes' frames among them; chunks go on every stream, and the reader puts them back in order by number. d, the first stream's two ways: Source to Destination, the manifest, the data and control; back, the answers, the window given back for the manifest and the data, and control.](../assets/protocol-frames.svg)

- **Format:** `[pipe: 1 byte][kind: 1 byte][length: 4 bytes, little-endian][content]`,
  its content at most 64 KiB. Once ready, a connection carries this one
  kind of packet alone. Its length takes four bytes, though two would
  almost do for 64 KiB, so that a frame's size may change without its
  format; its kind is a field apart from its pipe, plainer than giving the
  transport's frames pipe numbers of their own.
- **Kinds:**
  - `0 Bytes`: the pipe's next content;
  - `1 Window`: window given back for the pipe, its bytes, a u32;
  - `2 End`: the pipe written to its end, with no content; the data's with
    its count of chunks, a u64;
  - `3 Sync`: a sync, its number, a u32, and the bytes written on the pipe
    before it, a u64;
  - `4 Synced`: the answer of the reader's transport, once its program has
    read so far, the same number, a u32.
- **A pipe compressed as a stream:** a `Bytes` frame's content is the next
  of the pipe's zstd stream, or, not compressed, the pipe's bytes. Records,
  zstd's blocks and frames each have bounds of their own.
- **The data's chunks:** `[number: u64][0 as it is, 1 zstd: 1 byte][length: u32][content]`,
  little-endian, the header 13 bytes, each at most 2 MiB of the pipe. A
  chunk goes on one stream, cut into as many `Bytes` frames as it takes,
  other pipes' frames among them, a stream sending one chunk at a time; the
  reader's transport puts them back in order by number.
- **A chunk's number comes off the wire, and is not trusted:** one
  repeated, one past the count the data's `End` gave, or one further ahead
  of the chunk still awaited than the data's window can hold, its bytes
  over the fewest a chunk takes, breaks the protocol; nor may the next
  place overflow working it out. A block's number of old, `u64::MAX`,
  crashed the nightly fuzzer (2026-10-27, fixed in `fb6e6ec`).
- **Which stream:** the data's chunks go on all; every other pipe's frames,
  and every `Window`, `Sync` and `Synced`, on the first. `Window` and
  `Synced` go the other way from their pipe.
- **Sync:** a writer's `sync()` has the transport send `Sync`; once the
  reader's program has read so many bytes, its transport answers `Synced`,
  as it gives back window; `sync()` returns as `Synced` comes.
- **Order:** the pump of the first stream sends control, `Window` and
  `Synced` first; then the answers or the reports; then, of the data and
  the manifest, the data while a quarter of its send buffer is full, and
  the manifest otherwise.

### Compression

- Set for each connection, as [preparing carries](#what-preparing-carries):
  every pipe across the network, with no exception, none on this machine
  unless `--compress-level` says.
- **A pipe as a stream:** the manifest, the answers, the reports and
  control, each one zstd stream, its writer with one compressor's state,
  its reader one decompressor's.
- **The data:** in chunks of 2 MiB, compressed each alone by 8 threads, and
  decompressed so; a chunk that does not shrink goes as it is, and one not
  full is what was there as the data's turn came.
- **Flushing:** no waiting. The pump draws on the compressor: as a pipe's
  turn comes and there is room downstream, it takes all in the pipe's send
  buffer, at most a frame's worth, compresses it, and flushes it. Slow
  downstream, bytes gather and compress better; not slow, what is there
  goes. On a slow link a middling transfer sends somewhat more; see
  [Gathering a pipe's bytes on a slow link](future.md#gathering-a-pipes-bytes-on-a-slow-link).
- Integrity is not checked here: the files' BLAKE3 hashes, and the
  segments' chaining values, are the peers' to work out and check.

### Windows

- The manifest, the data, the answers and the reports have windows;
  control has none: its writer never waits for its reader, which takes all
  that comes. More than 16 MiB unread on control breaks the protocol, which
  is never reached otherwise.
- A window is the reader's to give, in bytes on the wire, frames' content,
  compressed.
- A writer whose window is used up waits; the reader's transport gives back
  what its program reads, a `Window` once a quarter of the window has
  gathered. `sync()` waits so too: where the far end does not read, it
  waits.
- The receiving thread never waits for a program: the far end never goes
  past the window, so its receive buffer always has room.
- The windows: the data's 64 MiB, the manifest's 2 MiB, the answers' 1 MiB,
  the reports' 1 MiB. A send buffer holds as much as its window, but the
  data's, 16 MiB.

### Ends and failures

- A writer closes its pipe once done, and the transport sends its `End`;
  its reader has all of it once it reads the `End`.
- The transport closes a connection once every pipe on it, both ways, has
  ended.
- A connection read to its end before each pipe the far end writes has
  ended is broken: the transfer fails, and nothing connects again. With K
  above 1, the streams after the first carry the data's chunks alone: one
  that ends within a chunk, or chunks fewer than the data's `End` says, is
  broken.
- Whether the far end is still there is the monitors' to judge, by their
  heartbeat; anything that comes on a connection counts as word from it;
  see [Monitors](architecture.md#monitors).
- When each connection ends: a server's peer, once done, says so, and
  waits for the client to end its control connection; the client's monitor
  ends both once both servers are done; the link closes once everything on
  the host has ended.
- **A peer's being done says only that it is done.** The one told judges
  from its own state whether it is done itself: the monitor, told, closes
  only the client's side; a worker has ended once its process has, and
  each pipe on its connection has read its `End`. So that the peer is done
  goes on control, and may come before or after the last reports.

## Messages

A packet is `[type: 1 byte][length: 4 bytes, little-endian][payload]`, its
payload the message's postcard encoding. Its types:

| Type | Message | Where |
|---|---|---|
| 0 | `Control` | a control connection, before ready and on its control pipe; a stream's control pipe |
| 5 | `Report` | a control connection's control and reports pipes |
| 11 | `Monitor` | a channel to the monitor, before ready; the link's control pipe |

Types 1, 2 and 3, a manifest's, an answer's and the data's records, are
packets only where tests and the fuzzer run a peer over one stream of
packets; on the pipes, they are records. 4 and 6 to 10 and 12 were used
before the pipes, and are not used again.

**`Control`**

| Message | From, to | What it says |
|---|---|---|
| `Hello` | client → server | the handshake; below |
| `Welcome { socket, port, root }` | server → client | it takes the `Hello`: the socket a stream reaches, or the port and its secret instead, and what its root is, the kernel's boot, the root's canonical path, whether it is there, its device and inode and each directory's above it, and the homes of the users it acts for |
| `Refused { reason }` | server → client | it refuses the `Hello`, and exits; the third variant, its reason alone, in every build |
| `Proceed { guard, streams }` | client → server | it may go on: what it must never go into or delete, and how many streams come |
| `Error { message }` | server → client, as its answer; peer → peer, on the stream | the sender failed, and stops |
| `Snapshot` | client → server | where are its threads |

`Control` also has `Done`, which nothing sends: a Destination's being done
goes in the answers, a server's on its control pipe as a report.

`Hello`'s fields, its build first, in every build:

| Field | What |
|---|---|
| `build` | the client's build, which the server's must be |
| `role` | `Source` or `Destination` |
| `root` | the server's root, as path bytes |
| `dry_run` | compare only, write nothing |
| `modify_window` | how close two mtimes must be to count as the same |
| `numeric_ids` | keep owners' numeric IDs, not mapped by name |
| `delete` | delete what the Source does not have: `ferry sync` |
| `one_file_system` | go into no directory on another filesystem: `-x` |
| `compress`, `compress_level` | whether this control connection's pipes are compressed, and at what level |
| `profile` | profile the run, and say where the time went |
| `batch_entries`, `backlog_files`, `paced_bytes`, `data_credit` | for tests and measurements; 0 for the defaults |
| `listers` | how many directories each side lists, and compares, at once; 0 for one |
| `cpus`, `place` | how many of the host's cores the server runs on, 0 for all, and where they are taken from |

**`Monitor`**

| Message | From, to | What it says |
|---|---|---|
| `Follow { compress }` | client → monitor, first | the connection is the link, its pipe compressed at `compress`, if any |
| `Launch { id, what }` | client → monitor, first | start `what` with the connection as its input and output: `Server { sudo, port }`, `SudoCheck { asks }` or `Fixture { arguments }` |
| `Ended { id, code, signal, said }` | monitor → client | how `id` ended, and, for what runs to its end, what it said on standard error |
| `NotStarted { id, reason }` | monitor → client | `id` could not be started |
| `Beat` | both | the far end is there, every 5 seconds |
| `Ping { id }`, `Pong { id }` | client → monitor, back | the round trip |
| `Stop` | client → monitor | stop what it started |
| `Paused { id }`, `Resumed { id }` | monitor → client | `id` was stopped, or went on |
| `Awaiting { ids }` | monitor → client | what it still waits for, told to stop |
| `Look`, `Looked { id, outside }` | client → monitor, back | how what it started looks from outside |

**`Report`**, from a server to the client: on its control pipe, `Metrics`
and `Carried`, every second, `Snapshot`, `Failed` and `Done`; on its
reports pipe, `Counts`, `Listed`, `Said`, `Queues`, `Transferring`, `Busy`,
`Levels`, and last `Profile`.

## What the pipes carry

**Decided.** What a program writes on a pipe is of two forms:

- **Packets,** on the pipes of messages, control and the reports, as
  before the pipes: `[type][length][payload]`, the type a byte, and the
  length four, little-endian. The handshake on a control connection, and
  what a connection to a host's monitor is for, before it is ready, are
  packets too.
- **Records,** on the manifest, the data and the answers: each thing its
  own, its length, a varint of at most five bytes, then its postcard
  encoding, at most 2 MiB and 64 KiB, as a segment of a file and what goes
  with it. A program writes as many in one write as it has, and the
  reader reads what came, keeping a record cut short for its next read.
  One longer, or that does not decode, breaks the protocol, and fails the
  transfer at once.

| Pipe | Connection | Direction | What it carries |
|---|---|---|---|
| control | the stream | both | an error, as a packet |
| manifest | the stream | Source → Destination | records: the root; for each directory, `In`, then its entries, by name, each its ID in turn, then `Listed`, or `Unlistable` for one not listed; owners' names before they are first used; paths that could not be read; the end |
| data | the stream | Source → Destination | records: a file whole, of up to 2 MiB, with its BLAKE3; a segment of 2 MiB of a larger one, with its chaining value; the hash of a file in segments, after its last; a file needed that could not be opened; the data's end |
| answers | the stream | Destination → Source | records: a file needed; attributes needed; the entries compared, from the first to the last; a failure; what `sync` could not delete; the needs' end; done |
| control | a server's control connection | both | packets: the client's go-ahead, and its asking where the threads are; the server's metrics and what the stream carried, every second, where its threads are, its failure, and that its peer is done |
| reports | a server's control connection | server → client | packets: what the peer reports, progress counts, how full its queues are, the files being transferred, paths a dry run lists, warnings, what could not be read or copied; last, where its threads' time went |
| control | the link | both | packets: the heartbeat, every 5 seconds each way, a ping for the round trip, a stop, a look; how each program the monitor started ended, when one was stopped or went on, what it still waits for, and how one looks from outside |

Each pipe flows as its content does, and none waits behind another but for
its window. While data is flowing, the Source keeps writing the manifest as
its scan finds files, and the Destination keeps writing its answers. What
must keep its order goes on one pipe: the hash of a file in segments after
its last segment, and a file that could not be opened, on the data; what
failed before the Destination's `Done`, on the answers. A program learns
that the other is done as its pipes end: each pipe's writer closes it once
it has written all, and its end goes after all of it. The data ends with
its own record, `End`, before the pipe's end, so that a pipe that ends
without it, as a Source's that failed do, is taken for cut, not for all the
data. The manifest ends so too.
