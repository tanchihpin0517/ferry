# TODO

Ferry's tracked work list. The design is in [docs/design/](docs/design/overview.md);
this list is the order in which it gets built.

**Incremental.** Each step extends the one before it and ends with something
that runs end to end and is tested. A step adds the least that makes the next
one possible, so early steps are deliberately crude: one thread, no
compression, few file types. Later steps replace the crude parts without
changing what already works.

**Tests need no SSH.** SSH only starts a child process and joins its standard
input and output, so the client can just as well start `ferry server` itself.
Tests use that, and a real host is needed only to try SSH itself.

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
- [ ] **5. Compare.** The Destination `stat`s each entry and replies with what
  it needs. `--dry-run` now prints only what would be transferred.
- [ ] **6. Transfer files.** Regular files and directories, in an uncompressed
  record stream. The Destination writes each file to a temporary name, checks
  its BLAKE3 hash, and renames it into place, resolving every path with
  `openat` and `O_NOFOLLOW`. This is the first step that copies data: `ferry
  ./a host:b` works for plain trees.
- [ ] **7. Everything in `rsync -a`.** Permissions, mtimes (directories last,
  deepest first), symlinks, devices, FIFOs, sockets, owner and group mapped by
  name through `getent` on Linux. Skip what cannot be set and summarise the
  warnings.
- [ ] **8. Durable and clean.** Batched `fsync`, on by default with a flag to
  turn it off. On error or Ctrl-C, remove the temporary files of the files not
  finished.
- [ ] **9. Results and the resend pass.** Each file's result goes back to the
  Source, and failed files are sent again in a final pass.
- [ ] **10. `--delete`.** Extras found per directory during the transfer,
  deleted only after the transfer succeeded, and listed by `--dry-run`.
- [ ] **11. Pull.** The peers swap sides: `ferry host:b ./a`.
- [ ] **12. Local copy.** Both peers in the client over an in-memory pipe:
  `ferry ./a ./b`.
- [ ] **13. Compression.** 2 MiB zstd frames, sent as soon as the stream goes
  idle, incompressible frames sent as is, and `--no-compress`.
- [ ] **14. Packet scheduling.** Data packets of at most 64 KiB, control packets
  first, and manifest, needs, and results interleaved with data.
- [ ] **15. Parallel walker.** Parallel read-ahead with a reorder buffer, so the
  manifest keeps its fixed depth-first order; bounded read-ahead.
- [ ] **16. Parallel stages.** Worker pools for compare, read and hash,
  compress, decompress, and write, joined by bounded queues; mimalloc.
- [ ] **17. Adaptive compression level.** Driven by the queue to the network;
  starts at 3, stays between 1 and 19, changes only after several frames
  agree. `--compress-level` fixes it.
- [ ] **18. Progress.** Counts as the walk goes and as files are sent, and a
  final summary.
- [ ] **19. Benchmarks.** Generated trees (many small files, a few large files,
  mixed), against rsync, locally and over SSH.
- [ ] **20. Reconnecting.** Retry with `BatchMode=yes`, then ask for the login
  in the terminal and continue; without a terminal, fail.
- [ ] **21. Remote to remote.** The client relays packets between two servers.
- [ ] **22. Releases.** A release workflow on `v*` tags: static musl builds for
  Linux on x86_64 and aarch64, macOS builds on both, and `SHA256SUMS`. The
  client installs a release build by downloading, checking, caching, and
  uploading the binary, and removes old builds from hosts.
- [ ] **23. User documentation.** Usage in the README, and a table of common
  rsync invocations and their Ferry equivalents.
- [ ] **24. More CI.** `shellcheck` over the scripts, and tests on other
  filesystems, such as XFS and Btrfs on loop devices.

## Not scheduled

- The typical workload and where rsync hurts in practice, the two open
  questions in the [overview](docs/design/overview.md#open-questions). The
  benchmarks in step 19 should use whatever the answers turn out to be.
- Proposed items in the design documents that are not yet decided, such as the
  CLI's shape and `-a` being the default. Each is settled at the step that
  first needs it.
- Later features: hardlinks, sparse files, xattrs, exclude patterns.
