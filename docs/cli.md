# Ferry CLI

[中文](cli.zh-TW.md)

How the `ferry` command behaves today. The [design](design/overview.md)
describes where it is going, and [TODO.md](../TODO.md) the order the rest is
built in. Anything not described here is not built yet.

## Synopsis

```text
ferry copy [OPTIONS] SOURCE DESTINATION
ferry sync [OPTIONS] SOURCE DESTINATION
ferry test [OPTIONS] SOURCE DESTINATION
ferry update [--check | --version VERSION]
```

| Command | Makes DESTINATION |
|---|---|
| `copy` | hold everything in SOURCE, keeping anything else it has, as `rsync -a SOURCE/ DESTINATION/` |
| `sync` | the same as SOURCE, deleting anything else it has, as `rsync -a --delete SOURCE/ DESTINATION/` |

Both copy SOURCE's contents into DESTINATION, as if SOURCE ended with a slash,
whether it does or not: from `data` to `backup`, `data/x` becomes `backup/x`,
never `backup/data/x`. rsync's distinction between `data` and `data/` does not
exist in Ferry.

`copy` copies what `rsync -a` does: regular files, directories, symlinks,
FIFOs, sockets, and devices, with their permissions (setuid, setgid, and
sticky included), mtimes to the nanosecond, and owners. `sync` does the same,
and then deletes from DESTINATION whatever SOURCE does not have.

Each file is written to a temporary name beside it, checked against the
source's BLAKE3 hash, and only then renamed into place, so a file at its final
name is always complete. A symlink, file, or empty directory where a
directory or file belongs is replaced. A directory with anything in it, where
a file belongs, is left alone by `copy`: that file fails, and the rest is
copied. `sync` removes the directory, with all it holds.

`sync` deletes only after everything else has been copied, and only if
nothing failed and all of SOURCE could be read. Otherwise something missing
from SOURCE's listing could be there after all, and `sync` deletes nothing and
says so:

```text
ferry: warning: deleted nothing, since some of the source could not be read
```

Deleting never follows a symlink: a link is deleted, not what it points to.

A file that changes while it is read is sent again, once, after the rest. If
it changes again, it fails. Each entry that fails is reported as it fails,
and the copy ends with a count of them and exit status 1:

```text
ferry: cannot copy notes.txt: a directory that is not empty is where this belongs
ferry: 1 entry could not be copied
```

Owners are mapped by name, as in rsync: the source sends the name of each
user and group ID it uses, once, and the destination gives the entry the ID
that name has there. A name that does not exist there, or one that was not
sent, keeps its number; `--numeric-ids` keeps every number. User and group 0
are never mapped: root is root everywhere. On Linux, names are looked up with
the system's `getent`, so accounts in LDAP or SSSD are found.

Without root, a process can give an entry only its own user, and only a group
it belongs to. Whatever else it cannot set, it leaves, without a word, as
rsync does: copying to a host as another user, without root, makes the files
that user's, with the source's group where the user belongs to it, and that
user's own group otherwise.

To keep every owner on a host, log in as root, or pass `--remote-sudo`: log in
as yourself, and the server on the host runs as root through `sudo`. Probing
and installing stay the login user's, so Ferry is installed in their home as
usual. If `sudo` needs a password, Ferry asks for it here, once a host,
without showing it, or takes it from `FERRY_SUDO_PASSWORD`; checks it on the
host; and gives it to each server's `sudo` on standard input (`sudo -k -S`),
so it is never on a command line. This machine's side is not affected: run
`sudo ferry` for that. A host whose sudo sets `requiretty` cannot be used so.

What is sent is compressed with zstd, in frames of up to 2 MiB that ignore
where files begin and end, so many small files compress together. A frame
that does not shrink, such as one of already compressed data, is sent as it
is. The level is 1, the fastest; `--compress-level` sets another, and
`--no-compress` sends everything as it is.

Before a file's temporary name is renamed, its content is `fsync`ed, and each
directory something was created in is `fsync`ed at the end, so a power loss
never leaves a file's name without its content, and one right after Ferry
finishes loses nothing. Files are renamed in batches, so that this costs
little even with many small files. `--no-fsync` skips it.

On macOS, `fsync` only hands data to the drive, which may reorder it in its
own cache. So before each batch is renamed, Ferry sets a barrier on the drive
(`F_BARRIERFSYNC`), and when the transfer is done, it has the drive empty its
cache once (`F_FULLFSYNC`). Copying 100,000 small files takes about 17 s this
way, against 15 s with `fsync` alone, which does not give the guarantee, and
over 500 s with `F_FULLFSYNC` on every file.

On Ctrl-C, or `SIGTERM` or `SIGHUP`, the temporary files of files not finished
are removed before Ferry exits. Files already renamed into place are complete,
so the next run sends only what is missing.

Sockets and devices that cannot be created without privilege are left out,
with a warning once, a count, and a few of the paths:

```text
ferry: warning: 2 entries were not created: creating sockets and devices needs privilege (dev/null, run/app.sock)
```

An owner that cannot be set is not counted as a difference, so it does not
make an entry look changed on every run.

```text
ferry copy --dry-run ./data ./backup
ferry copy --dry-run ./data workbox:/scratch/data
ferry copy --dry-run ./data me@workbox:data
```

## Locations

A location is a local path or a path on a host, written as scp and rsync write
it:

| Written as | Means |
|---|---|
| `path`, `./path`, `/abs/path` | a local path |
| `host:path`, `user@host:path` | `path` on `host`, over SSH |
| `host:` | the home directory on `host` |
| `host:relative` | `relative` to the home directory on `host` |

A colon before the first `/` makes the part before it a host. A local path with
such a colon, like `a:b`, is written `./a:b`. A host cannot start with `-`,
so it can never be read as an option to `ssh`. `host`
is anything `ssh` accepts, including a `Host` alias from `~/.ssh/config`.

Either SOURCE or DESTINATION, or both, may be on a host: copying to one is a
push, from one a pull, and between two, remote to remote, the data passing
through this machine. Whatever the sides, `ferry` itself prints what goes
wrong on either of them, so a pull reports what the host could not read, as a
push does.

## What is compared

SOURCE is a directory or a file. If it is a directory, its contents are
compared with DESTINATION's, whatever either is named: `ferry copy ./data
./backup` compares `./data/x` with `./backup/x`. If it is a file, it is
compared with DESTINATION itself.

A symlink named as SOURCE is followed. No other symlink is: a link inside
SOURCE is an entry of its own, compared as a link. A link inside DESTINATION is
never followed either, so nothing under a path through a link counts as
present.

An entry is listed when DESTINATION lacks it, or has something different:

| SOURCE has | Its content is unchanged when DESTINATION has |
|---|---|
| a file | a file of the same size and mtime |
| a directory | a directory |
| a symlink | a symlink with the same target |
| a FIFO or a socket | the same kind |
| a device | a device of the same kind and number |

Files are compared as rsync's quick check compares them: by size and mtime,
without reading their content. mtimes are compared in whole seconds by
default, as in rsync, because some filesystems and tools keep coarser times
than others: FAT keeps 2 seconds, HFS+ 1 second, and some copies drop the
nanoseconds. Comparing nanoseconds there would list the same unchanged files
on every run. `--modify-window` changes this.

An entry whose content is unchanged is still listed if its permissions, or an
owner Ferry could set, differ; a copy then sets them without sending any
content. A directory, symlink, or special file whose mtime alone differs is
not listed, but a copy sets its mtime. A directory's contents are compared
entry by entry, so an unchanged directory can still have changed entries
listed under it.

## Options

`copy` and `sync` take the same options.

| Option | Effect |
|---|---|
| `-n`, `--dry-run` | List what would be transferred, without copying anything |
| `--compress-level LEVEL` | Compress at this zstd level, from 1 to 19; 1 unless given |
| `--no-compress` | Send without compressing. Saves CPU on a network fast enough not to need it |
| `--progress` | Show progress and a summary even when standard error is not a terminal; on a terminal they are shown anyway |
| `--no-ui` | On a terminal, show the progress line instead of the dashboard |
| `--profile-file FILE` | Also write where the threads of the client and of each server spent their time to `FILE`, as JSON, with each state's histogram. See [profiling](design/profiling.md) |
| `--no-fsync` | Do not `fsync` files and directories. Faster, but a power loss soon after may lose what was copied |
| `--numeric-ids` | Keep owners' numeric user and group IDs, instead of mapping them by name. As rsync's option of the same name |
| `--modify-window SECONDS` | Count mtimes up to `SECONDS` apart as the same. `0`, the default, compares whole seconds; `2` suits FAT; `-1` compares nanoseconds too. As rsync's option of the same name |
| `-y`, `--yes` | Install Ferry on a host without asking |
| `--remote-sudo` | Run Ferry on a host as root, through `sudo`, so that it can keep every owner. As rsync's `--rsync-path="sudo rsync"`; see below |
| `--ssh-connections N` | Open N SSH connections to each host, each its own TCP connection, and spread the transfer over them; see [SSH connections](#ssh-connections). Default: 1 |
| `--remote-ferry PATH` | Run the Ferry binary at `PATH` on the host, instead of the build Ferry installs. `PATH` is relative to the home directory on the host unless it is absolute |
| `-h`, `--help` | Print help; `--help` prints more than `-h` |
| `-V`, `--version` | Print the version |

## Output

`--dry-run` writes one entry per line to standard output, in a fixed order:
the root first, then each directory's entries sorted by name, depth first.

- The root is `.`.
- A directory ends with `/`.
- Paths are relative to SOURCE, written as the bytes the filesystem holds,
  so a name that is not UTF-8 is printed as it is.

```text
$ ferry copy --dry-run ./data ./backup
.
a.txt
link
sub/
sub/b.txt
```

`sync --dry-run` also lists what it would delete, each directory's extras
after its other entries, as `deleting path`:

```text
$ ferry sync --dry-run ./data ./backup
deleting old.txt
deleting sub/stale/
```

An unchanged tree prints nothing.

On a terminal, a transfer shows a dashboard, a full screen that updates in
place: how far it has got, with the time left; the counts; each SSH
connection's rate and how long ago its last heartbeat came; how full the queue
before each stage of the pipeline is, and which stage holds the others back;
and the latest warnings. Ctrl-C cancels the transfer. When the transfer ends,
the terminal is put back as it was, and the summary, then every warning, is
written to standard error. See the [dashboard's design](design/dashboard.md).

With `--no-ui`, or for a dry run, standard error shows progress on a
line that updates in place instead, and a summary when the transfer is over:

```text
110,101 entries · 76,448 of 100,000 files · 74.7 KiB · 14.9 KiB/s
ferry: 110,101 entries compared, 100,000 files sent, 97.7 KiB in 5.4 s (18.2 KiB/s)
```

The entries counted are those scanned; the files, those whose content is
sent, out of those needed. Once compression saves something, the line says
how much. Scanning is usually faster than sending, so the totals settle early.
A dry run, which may list nothing for a long time, shows how far its
comparison has got instead, and what holds it back: the side whose threads
work the most, and at what. Its listing goes to standard output, written out
as it comes on a terminal, the line cleared before it:

```text
312,450 compared · 12 to send · 1:42 · Source on fatcat: list directories
```

Where standard error is not a terminal, nothing is shown, unless `--progress`
asks for it; then a line is written every few seconds instead of redrawn.

After the summary comes where the time went: for the client and each
server, each role of their threads on a line, with the states it spent its
time in. It shows which side, and which stage, held the transfer back. See
[profiling](design/profiling.md#the-profile).

Errors and warnings go to standard error, starting with `ferry:`. A path in
SOURCE that cannot be read is reported there, skipped, and counted.

On Ctrl-\, `SIGQUIT`, or when nothing has passed between the servers for
60 s, standard error says where every thread of the client and of each server
is, and for how long it has been there; the transfer goes on. See
[profiling](design/profiling.md#a-snapshot-of-every-thread).

## Exit status

| Status | Means |
|---|---|
| 0 | Every entry of SOURCE was read, compared, and, without `--dry-run`, copied |
| 1 | Something failed, or some paths in SOURCE could not be read |
| 128 + signal | Interrupted, such as 130 for Ctrl-C |

## Remote hosts

Ferry runs the system's `ssh`, so everything in `~/.ssh/config` applies:
aliases, `User`, `Port`, `ProxyJump`, identities, agents. A password or OTP is
asked for in the terminal, as `ssh` asks for it.

On the host, the client runs a `ferry server` of exactly its own build. The
server speaks to the client over the SSH connection, and exits when the
connection closes. A server of another build refuses the connection and says
which two builds met.

### Installing on a host

When the host does not have the client's build, the client installs it:

1. It probes the host over SSH for its machine ID, OS, architecture, installed
   builds, and `cargo`.
2. On a host where Ferry has never been installed, it asks first. Without a
   terminal to ask on, it refuses unless given `--yes`. A host that already
   has another build is not asked again.
3. A release build downloads its version's binary for the host's platform
   from GitHub Releases with `curl`, checks it against the release's
   `SHA256SUMS`, keeps it in `~/.ferry/releases/<version>/`, and uploads it
   over SSH. The host needs neither network access nor a toolchain. Releases
   are for Linux and macOS on x86_64 and aarch64; on any other host, use
   `--remote-ferry`.
4. A dev build uploads its source and builds it on the host with the host's
   `cargo`, so the host needs a Rust toolchain. Build output is shown as it
   happens. The client refuses if its source has changed since it was built:
   rebuild it, and run again.

Only a binary from a GitHub release is a release build. One built from
source, even with `cargo install` or `--release`, is a dev build.

A host keeps the three most recently used builds. Using or installing a
build removes the others.

Ferry never changes the host's `PATH`, and never runs a `ferry` found there.

### SSH connections

Each host is reached over SSH connections that are opened first, before
anything runs there, each only logged in, one at a time. A password or OTP is
asked for then, and never during the transfer.

- **One login per host.** The first SSH connection shares your own
  `ControlMaster`, if your SSH configuration sets one up for the host (as
  `ssh -G` shows), or else one of Ferry's own. Either way, a later `ssh` to
  the host, from the same run or a run in the next ten minutes, reuses it
  without logging in again.
- **`--ssh-connections N` opens N.** Each one after the first has a
  `ControlMaster` of Ferry's own, so each is its own TCP connection, and the
  transfer is spread over all of them; on a lossy path, several TCP
  connections together go faster than one. Each may need its own login: it is
  first tried without asking, which works with a key, and otherwise asked for
  in the terminal, saying which connection it is for. Without a terminal, or
  if a login fails, the run stops before anything is copied, saying which SSH
  connection and why. Ferry never quietly transfers over fewer than N.
- **A dropped SSH connection fails the transfer,** any of them, saying which.
  Each carries heartbeats both ways every 5 seconds, so one that goes silent,
  as when the network goes away or a laptop sleeps, is found within 30
  seconds, and one that closes, at once. A rerun skips the files already
  complete.

## Files

A release build and a dev build never share any of these: a release build
uses the names below, and a dev build the same names with `-dev`, such as
`~/.ferry-dev/`.

| Where | Path | Holds |
|---|---|---|
| client | `/tmp/ferry-<uid>/ssh/` | the shared SSH connections' sockets, mode 0700 |
| client, host | `/tmp/ferry-<uid>/join/` | while a server runs, the socket its path joins it by, mode 0700 |
| client | `~/.ferry/releases/<version>/` | downloaded release binaries, with their `SHA256SUMS` |
| host | `~/.ferry/hosts/<machine-id>/bin/<build>/ferry` | each installed build |
| host | `~/.ferry/hosts/<machine-id>/src/`, `build/` | a dev build's source and kept target directory |

The machine ID keeps apart the nodes of a cluster that share a home
directory. A host without one (`/etc/machine-id`, or `IOPlatformUUID` on
macOS) is refused.

## Environment

| Variable | Effect |
|---|---|
| `FERRY_HOME` | Ferry's directory, instead of `~/.ferry`. Must be absolute. On a host, set in the environment SSH gives the command |
| `HOME` | Where `~/.ferry` is, when `FERRY_HOME` is not set |
| `FERRY_SSH` | The program to run instead of `ssh`, given the same arguments, as rsync's `RSYNC_RSH` |

## `ferry update`

```text
ferry update [--check | --version VERSION]
```

Replaces this binary with the latest release, or with VERSION, older or
newer. It finds the latest release from where GitHub's `releases/latest`
leads, downloads the binary for this machine with `curl`, checks it against
the release's `SHA256SUMS`, and keeps it in `~/.ferry/releases/<version>/`,
as for a host, so that installing it on a host of the same platform later
costs no download. The binary is replaced by a rename in its own
directory, so an update that fails leaves the old one whole. Where that
directory is not writable, the update stops and says so.

Ferry never looks for a new release on its own; only `ferry update` does.
Hosts need nothing: the next transfer installs the new build there, as for
any build.

A dev build, built from source, refuses: update it from source.

| Option | Effect |
|---|---|
| `--check` | Only say whether a newer release is out: exit 2 if one is, 0 if not, 1 if the check failed |
| `--version VERSION` | Update to VERSION instead, such as `0.1.0` |

## `ferry test`

```text
ferry test [--yes] [--remote-ferry PATH] [--scale SIZE --kind KIND] SOURCE DESTINATION
```

Runs the whole flow from SOURCE to DESTINATION on trees it creates, and
removes them afterwards. SOURCE and DESTINATION are directories, local or
`[user@]host:path`, and may be the same directory. Inside them the test
creates `ferry-test-<id>-source` and `ferry-test-<id>-destination`, and
removes only those: removal refuses a directory without the `.ferry-fixture`
marker that every test tree has.

```text
$ ferry test /tmp workbox:/tmp
ferry test: from /tmp/ferry-test-aecd29714c78-source to workbox:/tmp/ferry-test-aecd29714c78-destination
phase 1: prepare
  ok    workbox: ready (0.41s)
  ok    copy: a missing source fails before anything changes (0.01s)
  ok    copy: a server that does not start fails before anything changes (0.04s)
  …
phase 2: transfer
  ok    copy: a missing destination needs everything (0.12s)
  ok    copy: an identical destination needs nothing (0.10s)
  …
  ok    sync: a directory in the way fails one file, unless syncing (0.13s)
  ok    sync: other permissions are restored (0.11s)
37 passed, 0 failed, 0 skipped
```

The source tree holds a file, nested directories, an empty directory, a
symlink, a dangling symlink, and a FIFO, with fixed contents and mtimes down
to the nanosecond. Each scenario puts a known tree at the destination, runs
the real `ferry` command between the two places, and checks what it prints.
Every scenario runs with every transfer command, `copy` and `sync`.

The scenarios come in the two phases of a run (see the
[pipeline](design/pipeline.md)):

1. **Prepare.** First each host is made ready, with Ferry installed on it as
   for a transfer. Then come scenarios that must fail while preparing, such as
   a missing source, or a server that does not start: each must fail with the
   right error and leave the destination's tree exactly as it was. A scenario
   that needs a server on a host is skipped when both sides are on this
   machine.
2. **Transfer.** Scenarios that compare and copy. They run only if nothing
   failed in the first phase.

A scenario that needs something not built yet is skipped and says why. Once a command is skipped, its other
scenarios are skipped with it. The scenarios grow as Ferry does: every step
adds its own, to the phase it belongs to.

On a host, the test trees are created and removed by Ferry itself, through a
hidden `ferry fixture` subcommand, so a host needs nothing but SSH. `--yes`
and `--remote-ferry` mean what they mean for a transfer.

The exit status is 0 when no scenario failed, and 1 otherwise.

### At scale

With `--scale SIZE`, such as `500M` or `20G` in binary units, and `--kind
KIND`, both needed, the test runs no scenarios. It transfers a tree of that
size and kind once, as a user would: on a terminal, each transfer shows its
dashboard, and its summary follows.

```text
$ ferry test /tmp workbox:/tmp --scale 1G --kind mixed
ferry test: 1.0 GiB of mixed, from /tmp/ferry-test-371f59084456c9-source to workbox:/tmp/ferry-test-371f59084456c9-destination
  ok    workbox: ready (0.82s)
  ok    generate: the source tree, 9,525 files in 579 directories, 46 symlinks, 1.0 GiB (7.42s)
ferry: 10,152 entries compared, 9,525 files sent, 1.0 GiB in 32.8 s (31.2 MiB/s) · 19% saved by compression
  ok    copy: the tree is received, at 31.1 MiB/s (32.91s)
  ok    copy: the destination matches the source (1.03s)
  ok    change: the source, 96 rewritten, 96 touched, 96 deleted, docs/d7/d1/d1/ removed, 212 files, 10.2 MiB added (0.24s)
ferry: 10,264 entries compared, 404 files sent, 14.3 MiB in 0.4 s (34.3 MiB/s) · 27% saved by compression
  ok    sync: the changes are received (0.53s)
  ok    sync: the destination matches the source (0.97s)
  ok    sync: nothing more is needed (0.15s)
8 passed, 0 failed
```

The steps run in order, and stop at the first that fails:

1. **Generate.** A tree of KIND at the source, with SIZE in its files (see
   below). The same KIND and SIZE make the same tree on every machine.
2. **Copy** it to the destination, which must then match the source.
3. **Change** the source, as a user would between two syncs: one directory
   removed, and of the files left, one in a hundred rewritten with other
   bytes of the same size, one given only an older mtime, and one deleted;
   and about 1% more added.
4. **Sync** it, and the destination must match the source again.
5. A dry run of the same sync must then list nothing.

The kinds are shaped as such data tends to be. File sizes spread as on real
filesystems, on a log-normal curve with a long tail; directories hold more
files or fewer, most of them few, and nest more or less deeply; and content
is text, which compresses as prose does, random bytes, which do not compress,
or blocks of the two in turn, which compress by about half.

| KIND | As | Files | Directories |
|---|---|---|---|
| `small` | source code, packages and environments, a dataset of small images | a few KiB each, 3% empty, a tenth with another's bytes, some symlinks; mostly text | deep, 8 files each by the median |
| `large` | checkpoints, videos, archives | 64 to 256 MiB each, mostly random, with a few small text files beside them | shallow, a few files each |
| `mixed` | a project, or a home directory | `code/` as `small`, a tenth of SIZE; `docs/`, files of all sizes up to 256 MiB, three tenths; `data/` as `large`, the rest | as each part |

At 1 GiB, `small` makes about 87,000 files in 5,000 directories, `large`
about 30, 8 of them large, and `mixed` about 9,500. `ferry fixture generate`, which the
benchmarks use, makes the same trees.

To match, the two trees must have the same digest: every entry's path, kind,
size, permissions, mtime to the nanosecond, and content, as the scenarios
compare them. Each side computes its own, through the hidden `ferry fixture
digest`, so nothing but the digest crosses the network.

After all the steps, both trees are removed. If a step fails, they are kept,
and their paths printed, to look into; remove them by hand.

The first Ctrl-C stops the test: the step running stops, on a host too, and
both trees are removed, whatever they were left with; the exit status is then
130. On the dashboard, Ctrl-C stops the transfer, and the test with it. A
second Ctrl-C exits at once, keeping whatever was not removed yet, at the
paths the first line gave.

## `ferry server`

A hidden subcommand: one side of a transfer. The client starts one for each
side, on a host over SSH or on this machine, and relays between the two. Its
standard input and output are its control connection: it shakes hands there,
and lives as long as that stays open. It then runs its peer over the path
that joins it: `ferry server --join <token>`, which hands its own standard
input and output to the server through a Unix socket, and waits until the
server is done. Neither is meant to be run by hand.
