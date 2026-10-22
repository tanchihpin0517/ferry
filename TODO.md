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
  Source, and failed files are sent again in a final pass. *Since replaced:
  a file is read up to the size it had when opened, one that cannot be read
  whole fails, and nothing is sent again (d58b3ae).*
- [x] **10. `ferry sync`.** Extras found per directory during the transfer,
  deleted only after the transfer succeeded, and listed by `--dry-run`.
  *Since replaced: extras are deleted as each directory is compared, and
  what the Source could not read is never deleted (ba3ed6e).*
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
  agree. `--compress-level` fixes it. Removed after the benchmarks of step
  23: the level is now fixed, at 1 by default.
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
- [x] **21. Transport.** Under the client and its two servers, a transport
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
- [x] **22. Several SSH connections.** Inside the transport only. One SSH
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
- [x] **23. Benchmarks.** Generated trees (many small files, a few large files,
  mixed), against rsync, locally and over SSH.
- [x] **24. Heartbeats and path ends.** Each SSH connection gets a heartbeat
  channel beside its path, joining the server alike; both sides send a
  heartbeat on each every 5 seconds, and 30 seconds of silence, or the
  channel ending while the connection is open, fails the connection, saying
  which SSH connection. The local transport has them too. Each path ends
  with an end marker, and one that ends without it fails the connection.
  Tested alone: a path ending without its marker, a heartbeat channel
  falling silent, with shortened times, and one ending; and by `ferry test`,
  a transfer with several SSH connections, one of them cut.
- [x] **25. Dashboard.** A full-screen view of the transfer, on by default
  on a terminal, as the [dashboard](docs/design/dashboard.md) describes: the
  whole transfer, the counts, each SSH connection's rate and heartbeat, the
  pipeline's queues and its bottleneck, and the latest warnings. The
  servers report their queues in a new kind of report. `--no-ui`
  shows the progress line instead; a dry run, and output that is not a
  terminal, are as before. The terminal is always put back, and warnings
  are written out when the dashboard closes. `ratatui` over `crossterm`.
- [x] **26. User documentation.** Usage in the README, and a table of common
  rsync invocations and their Ferry equivalents.
- [x] **27. More CI.** `shellcheck` over the scripts, and tests on other
  filesystems, such as XFS and Btrfs on loop devices: `make shellcheck`,
  part of `make check`, and `make filesystem-test FS=…`, which runs the
  tests and `ferry test` on a loop device, each a CI job. `make test` runs
  the tests with cargo-nextest where it is installed, as CI has it, each
  with a time limit, so that one that hangs fails by name.
- [x] **28. Releases.** A release workflow on `v*` tags, which must be the
  version in `Cargo.toml`: static musl builds for Linux on x86_64 and
  aarch64, macOS builds on both, each tested and run on a runner of its
  architecture, published uncompressed with `SHA256SUMS`, and an
  `install.sh` for `curl … | sh`, tested on each runner. Only that workflow
  builds a release build, by setting `FERRY_RELEASE`. A release client
  installs on a host by downloading its version's binary with `curl`,
  checking it, keeping it in `~/.ferry/releases/<version>/`, and uploading
  it. A host keeps the three most recently used builds. `v0.1.0` is
  published, and installs itself on a Linux x86_64 host.
- [x] **29. Updating.** `ferry update` replaces a release binary with the
  latest release, or `--version VERSION`, downloaded and checked as for a
  host and kept with those, by a rename in its directory; `--check` only
  says whether a newer one is out, exiting 2 if so. A dev build refuses.
  Ferry never looks for a release on its own.
- [x] **30. Large trees.** A walk that fails stops the transfer with its
  reason instead of hanging. A directory of more than 10,000 entries is
  sent in parts. Read-ahead is bounded by entries, about a million, and the
  manifest by 64 MiB waiting to be sent. The Source keeps each entry's path
  compactly. On a tree of 10 million entries, with ten directories of a
  million, a dry run takes about 10 s, with the Source at about 620 MB.
- [x] **31. Profiling.** Each thread registers with a role and is always in
  one state, changed only where work passes: pools, queues, the mux, the
  walk, the transport, and the relay. `--profile` says at the end where each
  role's time went, and `--profile=FILE` writes it all as JSON. Ctrl-\, or
  60 s with nothing passing between the servers, says where every thread is.
  It costs about 1% more CPU with `--profile`, and nothing measurable
  without; see [profiling](docs/design/profiling.md).
- [x] **32. Scan.** The walk becomes the scan: one thread lists directories,
  depth first, and 8 stat their entries in order, so the filesystem is
  asked at most one listing and 8 stats at once. The Destination scans its
  own tree the same way, following the manifest: it opens and lists each
  directory once, merges it with the Source's parts, finds what `sync`
  deletes from the same listing, and goes into nothing the Source does not
  have. On NFS, it halves the server's busiest load, and cuts its calls by
  30 to 50% when both sides are there, for a scan 1.5 to 2 times slower; on
  a local disk, a dry run of 10 million entries goes from 9.4 s to 6.6 s.
  The lines a dry run lists go a part's at a time, in reports of at most
  1 MiB, rather than a packet each, and wait, as the manifest does, while
  64 MiB of messages wait to be sent: the same dry run then takes 3.0 s,
  with the Destination at 90 MB rather than 150 MB.
- [x] **33. Profiling always on.** Profiling is part of Ferry, not an
  option: only a hidden `--no-profile` turns all of it off, for measuring
  what it costs. Each server sends its metrics every second on its control
  connection, and the client takes its own and keeps each side's last 10
  seconds, for a policy to steer the transfer by; none steers anything yet.
  `--profile` is gone: where the time went follows the summary, shortened,
  wherever the summary is shown, and `--profile-file FILE` writes all of it
  as JSON. Measured against `--no-profile`, no difference shows.
- [x] **34. Guards.** Each server tells the client what its root is, by
  device and inode, with each directory above it, and waits. A DESTINATION
  that is SOURCE, is inside it, or holds it is refused before anything
  changes, for `copy` and `sync`; `sync` into `/`, a home directory, or a
  directory that holds one, such as `host:` alone, asks first, or without a
  terminal needs `--allow-dangerous-destination`, which `--yes` is not.
  The client then tells each server to go on, with what it must never go
  into or delete: the Source's scan stops at the Destination's root, and the
  Destination neither goes into the Source's root nor deletes it or a
  directory above it. `host:~` and `host:~/path` are the home directory, as
  for scp. Roots on hosts that share a network filesystem are not compared
  yet. Review items CLI-01, CLI-04, and DOC-01.
- [x] **35. Output and exit status.** Where the time went is said only
  with `--profile`, as step 33 said it after every summary (D8). A transfer
  that ran to its end exits with 23 where some entry failed, and 24 where
  only some paths in SOURCE could not be read, or vanished, as rsync's do;
  `update --check` exits with 3, and a wrong command line with 2 (D7).
  Names are written safe to print, and a dry run lists with `-0` for
  scripts. Review items CLI-03, CLI-06, CLI-07, CLI-08, CLI-10, and DOC-05.
- [x] **36. Tests as root.** What only root can do is tested as root, in a
  CI job of its own, `make root-test`: owners mapped by name, through a
  fake `getent` that answers as two machines, or kept by number with
  `--numeric-ids`; root never mapped; setuid bits kept through the change
  of owner; devices and sockets made; an owner change alone listed and set
  right; and a Destination bind-mounted inside the Source never gone into.
  Elsewhere each test says it is skipped. Review item TST-02.
- [x] **37. Fuzzing.** Fuzz targets, in `fuzz/`, a crate of its own, for
  what a peer may send: packets, frames, records, a striped connection's
  pieces, `getent`'s lines, and the Destination given a manifest and a
  record stream, after which nothing beside its root may have changed.
  They build in CI on every push, `make fuzz-build`, and each runs five
  minutes every night, `make fuzz FUZZ=…`, from the corpus the nights
  before found. Round trips of frames, records, pieces and printable names
  are tested over many seeded inputs. Review item TST-03.
- [x] **38. Actions pinned.** Every action a workflow uses is pinned by
  its commit, with its version beside it, and Dependabot proposes newer
  ones, and newer crates, every week. A release is built without the build
  cache. Review item CI-03.
- [x] **39. Dependencies checked.** `make deny`, part of `make check` and
  a CI job, holds the dependencies to `deny.toml`: no known advisory, nor a
  yanked crate, no license but those that ask only for their notice, and
  nothing from outside crates.io; the advisories are checked every night
  too. A release publishes `THIRD-PARTY-LICENSES.txt`, `make licenses`,
  with every crate's license and musl's. Review item CI-02.
- [x] **40. One filesystem.** `-x`, `--one-file-system`, as rsync's: the
  scan sends a mount point, as a directory, but not what it holds, and
  `sync` deletes nothing on another filesystem than the Destination's
  root, keeping a directory that holds one, with a warning. Without it,
  both go into mount points, as rsync's do, which the docs now say. A stat
  mounts nothing an automounter would. Review items SRC-09 and DST-19
  (D10).

## Set aside

Steps taken out of the order for now, to come back to later.

- [ ] **Reconnecting.** Inside the transport. Retry with `BatchMode=yes`,
  then ask for the login in the terminal and continue; without a terminal,
  fail. Covers every connection, not only the first. Until then, a dropped
  connection fails the transfer, as it does for rsync, and a rerun skips the
  files already complete.
- [ ] **Adaptive compression level.** The level fixed at 1 until then. The
  first adaptive level, driven by the queue to the network, was too slow,
  several times slower than a fixed level in the benchmarks of step 23, and
  was removed. How to do better
  is to be worked out then. One idea: measure how many bytes go through,
  before compression, and take back a step that did not make that faster,
  and do not raise the level for frames that do not shrink.

## Not scheduled

- The typical workload and where rsync hurts in practice, the two open
  questions in the [overview](docs/design/overview.md#open-questions). The
  benchmarks in step 23 should use whatever the answers turn out to be.
- Proposed items in the design documents that are not yet decided, such as the
  CLI's shape and `-a` being the default. Each is settled at the step that
  first needs it.
- Later features: hardlinks, sparse files, xattrs, exclude patterns, and
  `--checksum`.
- Policies that steer a transfer by what the client follows of it, and the
  orders the client sends a server on its control connection to carry them
  out, with each server's own hard limits: the compression level first,
  then the threads of the stages bound by CPU. The adaptive compression
  level set aside above would be the first.
- The Destination holds a directory's listing until its last part, about
  75 MB for a directory of a million entries. Its names could be kept in
  one buffer, as the Source's known entries are.
