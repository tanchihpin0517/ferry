# TODO

Ferry's tracked work list. The design is in [docs/design/](docs/design/overview.md);
this list is the order in which it gets built.

**Incremental.** Each step extends the one before it and ends with something
that runs end to end and is tested. A step adds the least that makes the next
one possible, so early steps are deliberately crude: one thread, no
compression, few file types. Later steps replace the crude parts without
changing what already works.

**Every step is covered by `ferry test`.** A step adds the scenarios that
exercise what it built, for every command it affects, in the phase they
belong to, preparing or transferring, and is done only when `ferry test`
passes locally and against a real host.

**Tests need no SSH.** The local transport starts servers on this machine,
through the same steps as the SSH transport except logging in, so `ferry test`
between local directories runs the whole flow. A real host is needed only to
try SSH itself.

Legend: `[ ]` open, `[~]` in progress, `[x]` done.

## The steps

- [x] **1. Connect.** The client starts `ferry server` on a host with the
  system's `ssh`, or directly as a child process for local runs and tests.
  They exchange packets (`[type][length][payload]`, control only for now)
  and a handshake with the build identity, and a mismatch is refused with a
  clear error. The server exits when its input closes. The remote binary is
  given by `--remote-ferry` until step 2.
- [x] **2. Install on the host.** Probe the host's machine ID, OS,
  architecture, and installed builds. Install a dev build by uploading the
  source and building it there, asking before the first install, or not with
  `--yes`. From here, connecting to a new host just works.
- [x] **3. Shared SSH connection.** The managed ControlMaster options, and the
  setting that turns them off, so a login or OTP is typed once per host.
- [x] **4. Send a manifest.** The Source walks its tree, one thread, one
  directory at a time, sorted by name, without following symlinks, and sends
  the manifest. `--dry-run` prints what the Destination received.
- [x] **5. Compare.** The Destination `stat`s each entry and replies with what
  it needs. `--dry-run` now prints only what would be transferred.
- [x] **6. Transfer files.** Regular files and directories, in an uncompressed
  record stream. The Destination writes each file to a temporary name, checks
  its BLAKE3 hash, and renames it into place, resolving every path with
  `openat` and `O_NOFOLLOW`. This is the first step that copies data: `ferry
  copy ./a host:b` works for plain trees.
- [x] **7. Everything in `rsync -a`.** Permissions, mtimes (directories last,
  deepest first), symlinks, devices, FIFOs, sockets, owner and group mapped by
  name through `getent` on Linux. Skip what cannot be set and summarise the
  warnings.
- [x] **8. Durable and clean.** Batched `fsync`, on by default with a flag to
  turn it off. On error or Ctrl-C, remove the temporary files of the files not
  finished.
- [x] **9. Results and the resend pass.** Each file's result goes back to the
  Source, and failed files are sent again in a final pass.
- [x] **10. `ferry sync`.** Extras found per directory during the transfer,
  deleted only after the transfer succeeded, and listed by `--dry-run`.
- [x] **11. Pull.** The peers swap sides: `ferry copy host:b ./a`.
- [x] **12. Local copy.** Both peers in the client over an in-memory pipe:
  `ferry copy ./a ./b`.
- [x] **13. Compression.** 2 MiB zstd frames, sent as soon as the stream goes
  idle, incompressible frames sent as is, and `--no-compress`.
- [x] **14. Packet scheduling.** Data packets of at most 64 KiB, control packets
  first, and manifest, needs, and results interleaved with data.
- [x] **15. Parallel walker.** Parallel read-ahead with a reorder buffer, so the
  manifest keeps its fixed depth-first order; bounded read-ahead.
- [x] **16. Parallel stages.** Worker pools for compare, read and hash,
  compress, decompress, and write, joined by bounded queues; mimalloc.
- [x] **17. Adaptive compression level.** Driven by the queue to the network;
  starts at 3, stays between 1 and 19, changes only after several frames
  agree. `--compress-level` fixes it.
- [x] **18. Progress.** Counts as the walk goes and as files are sent, and a
  final summary.
- [x] **19. Reports to the client.** The peers report to the client in
  messages, instead of counting and printing in the client's process: progress
  counts, the paths a dry run lists, warnings, what could not be read or
  copied, and the summary. The client shows them, and `progress::COUNTS` goes.
  Nothing else changes, and `ferry test` passes unchanged.
- [x] **20. The client and two servers.** The client stops running a peer, as
  the [architecture](docs/design/architecture.md#client-and-servers)
  describes: for each side it starts a server, a child process on this machine
  or over SSH on a host, with the connections built so far, and relays between
  the two, passing every message on and showing the reports. Both peers always
  run in servers, so push, pull, local copy, and remote to remote are one
  flow, and remote to remote works from here. `ferry test` passes for all
  four, and gains the scenarios of remote to remote.
- [ ] **21. Transport.** Under the client and its two servers, a transport
  module that owns every SSH connection and gives two kinds of abstract
  connection, as the [architecture](docs/design/architecture.md#transport)
  describes, each opened by naming the program at the far end. A control
  connection is a plain byte stream to one program: the probe, the install,
  the fixtures of `ferry test`, and the server, which lives as long as its
  control connection and shakes hands over it. A connection carries whole
  messages, in order, with `send` and `recv`, and waits when the far side
  cannot keep up; its path runs `ferry server --join`. Two transports: SSH,
  and local, which starts programs on this machine. One SSH connection and one
  path for now, so nothing above the transport changes, and `ferry test`
  passes unchanged. The transport is tested alone: messages whole and in
  order, backpressure, the end of a connection, a program that fails, and the
  server ending with its control connection.
- [ ] **22. Several SSH connections.** Inside the transport only. One SSH
  connection by default, sharing the ControlMaster of your own SSH
  configuration when `ssh -G` shows one, and Ferry's own otherwise;
  `manage_control_master` is removed. `--ssh-connections N`, 1 by default,
  opens exactly N, the ones after the first each over a ControlMaster of
  Ferry's own, so each is its own TCP connection, and every path joins the
  server alike. Messages are cut into numbered pieces, each goes to the path
  with the shortest queue, and they are put back together, in order, on the
  far side. Every SSH connection is opened and logged in first, before
  probing, installing, or starting a server, one at a time: `BatchMode=yes`
  first, then a login in the terminal. If any cannot be opened, without a
  terminal or after a failed login, preparing fails and says which and why.
  Tested alone over several in-memory paths, one slowed down, and by `ferry
  test` with several SSH connections, and with one that cannot log in.
- [ ] **23. Benchmarks.** Generated trees (many small files, a few large files,
  mixed), against rsync, locally and over SSH.
- [ ] **24. Reconnecting.** Inside the transport. Retry with `BatchMode=yes`,
  then ask for the login in the terminal and continue; without a terminal,
  fail. Covers every connection, not only the first.
- [ ] **25. Releases.** A release workflow on `v*` tags: static musl builds for
  Linux on x86_64 and aarch64, macOS builds on both, and `SHA256SUMS`. The
  client installs a release build by downloading, checking, caching, and
  uploading the binary, and removes old builds from hosts.
- [ ] **26. User documentation.** Usage in the README, and a table of common
  rsync invocations and their Ferry equivalents.
- [ ] **27. More CI.** `shellcheck` over the scripts, and tests on other
  filesystems, such as XFS and Btrfs on loop devices.

## Not scheduled

- The typical workload and where rsync hurts in practice, the two open
  questions in the [overview](docs/design/overview.md#open-questions). The
  benchmarks in step 23 should use whatever the answers turn out to be.
- Proposed items in the design documents that are not yet decided, such as the
  CLI's shape and `-a` being the default. Each is settled at the step that
  first needs it.
- Later features: hardlinks, sparse files, xattrs, exclude patterns, and
  `--checksum`.
