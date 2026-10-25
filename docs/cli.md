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
exist in Ferry. A trailing slash on DESTINATION means nothing either: a file
copied onto `new/`, where there is nothing yet, becomes the file `new`, where
rsync would make `new` a directory and copy the file into it.

`copy` copies what `rsync -a` does: regular files, directories, symlinks,
FIFOs, sockets, and devices, with their permissions (setuid, setgid, and
sticky included), mtimes to the nanosecond, and owners. `sync` does the same,
and then deletes from DESTINATION whatever SOURCE does not have.

Each file is written where nothing sees it, checked against the source's
BLAKE3 hash, and only then given its name, so a file at its final name is
always complete: on Linux, where the filesystem can, it has no name until
then; elsewhere, as on macOS or NFS, it is written to a temporary name beside
it, and renamed into place. A symlink, file, or empty directory where a
directory or file belongs is replaced. A directory with anything in it, where
a file belongs, is left alone by `copy`: that file fails, and the rest is
copied. `sync` removes the directory, with all it holds, and a dry run lists
it as `deleting name/`.

SOURCE and DESTINATION themselves are looked at first, before anything
changes. `sync` works only from a directory into a directory, or into a
DESTINATION that is not there yet; a file is refused, with a pointer to
`copy`. `copy` refuses a directory onto anything but a directory, and copies a
file onto a directory into it, as `DESTINATION/NAME`, as rsync does.

SOURCE and DESTINATION may not overlap. A DESTINATION that is SOURCE, is
inside it, or holds it is refused, for `copy` and `sync` alike, and in a dry
run too: `ferry sync a/sub a` would delete the Source, and `ferry copy a
a/sub` copy it into itself. The two are compared as their kernel knows them,
by device and inode, so a symlink or another spelling of the same directory
is refused as well. The check is made again as the transfer goes, so that a
mount that makes one appear inside the other stops it. Two hosts that share a
network filesystem, such as NFS, are not compared yet: a DESTINATION on one
inside a SOURCE on the other is not caught.

`sync` asks before it deletes in `/`, in a home directory, or in a directory
that holds one, such as `/home`, or `host:` alone, which is the home
directory on `host`: easy to name by mistake, and a sync there deletes
everything SOURCE does not have. Without a terminal to ask on, it is
refused unless given `--allow-dangerous-destination`; `--yes` does not agree
to it. A dry run only warns. `copy` deletes nothing, and is not asked about.
A mount point, such as `/mnt/backup`, is not asked about either.

`sync` deletes as it goes, a directory at a time, as soon as it has compared
it, as rsync does by default. What SOURCE lists but cannot read is still in
its listing, as unreadable: DESTINATION's entry of that name is left as it
is, neither copied over nor deleted, and the path is reported. A directory
SOURCE cannot list has nothing listed, so nothing in it is deleted either.

A transfer that stops partway may so leave DESTINATION with less than it
had: what was deleted, and not yet what replaces it, as when a file moved
from a directory compared early to one compared late. SOURCE is never
changed; running `sync` again completes it.

Deleting never follows a symlink: a link is deleted, not what it points to.

A file is read up to the size it had when it was opened, as rsync does: what
it gains while it is read is left for the next run, which finds it changed.
One changed in place while it is read, keeping its size, is found changed by
the next run only if its mtime moved by more than `--modify-window`: by
default, a change within the same second as the mtime it had when it was
opened goes unnoticed, and the copy keeps what was read, old and new
together, as rsync's does; `--modify-window -1` compares nanoseconds. One
that shrinks while it is read, or whose reading fails, fails;
nothing is sent again. Each entry that fails is reported as it fails, and
the copy ends with a count of them and exit status 23, as rsync's partial
transfer:

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

What is sent is compressed with zstd, in blocks of up to 2 MiB, each on
its own: many small files in one, so that they compress together, or a
piece of a large file. A block that does not shrink, such as one of already
compressed data, is sent as it is. The level is 1, the fastest;
`--compress-level` sets another, and `--no-compress` sends everything as it
is. Between two sides on this machine, where nothing crosses a network,
nothing is compressed, unless `--compress-level` asks for it.

Ferry does not `fsync` what it writes, as rsync does not unless given
`--fsync`: the system writes it to storage when it will, on Linux by default
within about half a minute. A power loss, or a crash of the system, soon
after a transfer may lose what it wrote: a file at its name may be empty, or
hold only part of its content, with the old one it replaced gone too. Ferry
stopping, however it stops, loses nothing so: what it wrote is in the
system's cache. On NFS, closing a file waits for the server to store it,
and a file that fails to close, as on a full drive there, fails, as
rsync's does; an error met writing back later, as a failing disk, goes
unseen, and the file is counted copied. Where a power loss right after
must not lose it, run `sync` on the Destination's machine after the
transfer: on Linux, it waits until everything is on storage. `--fsync` is
not built yet, and is refused.

On Ctrl-C, or `SIGTERM` or `SIGHUP`, the temporary files of files not finished
are removed before Ferry exits, and the `ssh` it started to hold each SSH
connection is ended, as they are when the transfer fails, as when the Source
fails or a connection drops; but not when Ferry is killed with `SIGKILL`. A file written without a name leaves
nothing behind, even when Ferry is killed. Files already in place are
complete, so the next run sends only what is missing.

Sockets and devices that cannot be created without privilege are left out,
with a warning once, a count, and a few of the paths:

```text
ferry: warning: 2 entries were not created: creating sockets and devices needs privilege (dev/null, run/app.sock)
```

So, one warning per kind, are an owner, permissions, or an mtime that could
not be set, as on an entry another user owns, where root is squashed, or on
FAT, or an owner with no ID here, as in a container: the entry is still
copied. An entry whose owner root could not set loses its setuid bit, and
one whose group it could not set, its setgid bit, with a warning of its own:
it would otherwise run as root here, which the Source never let it. rsync
then leaves the permissions and mtime unset altogether. What cannot be set
is not counted as a difference, so it does not make an entry look changed on
every run.

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
| `[address]:path`, `user@[address]:path` | `path` on the host at an IPv6 address, as scp and rsync write it |
| `host:` | the home directory on `host`, which `sync` asks about first |
| `host:relative` | `relative` to the home directory on `host` |
| `host:~`, `host:~/relative` | the same as `host:` and `host:relative`, as scp and rsync read them |
| `host:./~/relative` | a directory named `~` in the home directory on `host` |

A colon before the first `/` makes the part before it a host. A local path with
such a colon, like `a:b`, is written `./a:b`. Another user's home directory,
`host:~user/path`, is not read yet, and is refused: write the path in full.
A local `~` is your shell's to expand, as it does before Ferry sees it. A host cannot start with `-`,
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

A symlink named as SOURCE is followed if it points to a directory, as rsync
goes into `SOURCE/`; one to a file, or to nothing, is copied as a symlink,
as rsync copies one. No other symlink is followed: a link inside
SOURCE is an entry of its own, compared as a link. A link inside DESTINATION is
never followed either, so nothing under a path through a link counts as
present. DESTINATION itself, a symlink to a directory, is gone into; a
symlink to anything else is replaced, as rsync does, and what it points to
is left as it is.

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
| `-0`, `--null` | With `--dry-run`, list each entry as `+ PATH`, or `- PATH` to delete, its bytes as they are, ending in a NUL: for a script |
| `--compress-level LEVEL` | Compress at this zstd level, from 1 to 19; 1 unless given. Given, a copy between two sides on this machine compresses too, which otherwise it does not |
| `--no-compress` | Send without compressing. Saves CPU on a network fast enough not to need it |
| `--progress` | Show progress and a summary even when standard error is not a terminal; on a terminal they are shown anyway |
| `--no-ui` | On a terminal, show the progress line instead of the dashboard |
| `--profile` | After the summary, say where the threads of the client and of each server spent their time: which side, and which stage, held the transfer back. See [profiling](design/profiling.md) |
| `--profile-file FILE` | Also write where the threads of the client and of each server spent their time to `FILE`, as JSON, with each state's histogram. See [profiling](design/profiling.md) |
| `--fsync` | Not built yet: refused, with exit status 2. Ferry does not `fsync` what it writes; see [Synopsis](#synopsis) |
| `--no-fsync` | Does nothing, as nothing is fsynced: taken so that a command line written for Ferry 0.16 or before, which synced unless given it, still runs. rsync has no such option |
| `--numeric-ids` | Keep owners' numeric user and group IDs, instead of mapping them by name. As rsync's option of the same name |
| `-x`, `--one-file-system` | Stay on the filesystem each root is on. The scan goes into no directory on another, a mount point: the mount point is copied, as a directory, but not what it holds, and what the Destination has in it is left as it is. `sync` deletes nothing in a directory on another filesystem than the Destination's root, nor such a directory; one that holds such a directory is kept, with it, and the rest of what it holds is deleted, which a warning says. A dry run lists that one as to be deleted all the same. Without `-x`, both go into mount points, as rsync's do. As rsync's `-x`, given once. rsync's `-xx`, which leaves mount points out altogether, so that `sync` deletes what the Destination has at their names, is not built: `-x` given twice is refused |
| `--modify-window SECONDS` | Count mtimes up to `SECONDS` apart as the same. `0`, the default, compares whole seconds; `2` suits FAT; `-1` compares nanoseconds too; no other negative number is taken. As rsync's option of the same name |
| `-y`, `--yes` | Agree without asking to install Ferry on a host |
| `--allow-dangerous-destination` | Sync into DESTINATION without asking, even when it is `/`, a home directory, or a directory that holds one, such as `host:` alone; see [Synopsis](#synopsis). `--yes` does not agree to this |
| `--remote-sudo` | Run Ferry on a host as root, through `sudo`, so that it can keep every owner. As rsync's `--rsync-path="sudo rsync"`; see below |
| `--cpus N\|all` | Run on at most N cores of each machine, the client's and each host's, or on all of them; see [Cores](#cores). Default: 8 |
| `--remote-ferry PATH` | Run the Ferry binary at `PATH` on the host, instead of the build Ferry installs. `PATH` is relative to the home directory on the host unless it is absolute |
| `--remote-join` | Have the stream to a host, and its server's control connection, join its server by running Ferry there, through the shell on the host, rather than reach it through SSH: for a host whose `sshd` will not forward to a Unix socket (`AllowStreamLocalForwarding no`). A program the shell's startup files leave running may then write into the stream; see [architecture](design/architecture.md#connections) |
| `-h`, `--help` | Print help; `--help` prints more than `-h` |
| `-V`, `--version` | Print the version |

## Output

`--dry-run` writes one entry per line to standard output, in a fixed order:
the root first, then each directory's entries sorted by name, depth first.
On a terminal, where the dashboard shows the run, the listing is not
written there, so that thousands of lines do not flood it: the dashboard
counts them and shows the latest few, and the end says how many there were.
Redirect standard output to see them, as with `> file` or `| less`, or pass
`--no-ui` to have them written to the terminal as they come.

- The root is `.`.
- A directory ends with `/`.
- Paths are relative to SOURCE. Bytes that a terminal would act on, such as
  a newline or ESC, and bytes that are not UTF-8, are written as `\#` and
  three octal digits, as rsync writes them, and so is a name's first byte
  where the name starts with `deleting `, so that no name can forge a line.
  `-0` lists each entry as `+ PATH` or `- PATH` and a NUL instead, with its
  bytes as they are.
- A reader that stops early, as `head` does, ends the listing quietly, with
  status 0.

```text
$ ferry copy --dry-run ./data ./backup
.
a.txt
link
sub/
sub/b.txt
```

`sync --dry-run` also lists what it would delete, as `deleting path`, each
directory's extras before its other entries, in the order a run takes:
a run deletes what a directory has extra before it copies anything into it,
as rsync does, so that nothing it copies is ever taken for extra:

```text
$ ferry sync --dry-run ./data ./backup
deleting old.txt
deleting sub/stale/
```

An unchanged tree prints nothing.

On a terminal, a transfer shows a dashboard, a full screen that updates in
place: how far it has got, with the time left; the counts; each SSH
connection's rate and how long ago anything came on it; how full the queue
before each stage of the pipeline is, and which stage holds the others back;
and the latest warnings. Ctrl-C cancels the transfer. When the transfer ends,
the terminal is put back as it was, and the summary, then every warning, is
written to standard error. See the [dashboard's design](design/dashboard.md).

Before the transfer starts, a line on standard error says what the client
is doing, with the time it has taken, once a step takes a moment: logging in
to a host, checking for Ferry there, downloading and uploading a release to
install, or starting a side's server. A dev build installed on a host is
built there with cargo, whose own progress shows instead:

```text
ferry: checking Ferry on fatcat… 4 s
ferry: uploading Ferry to fatcat: 3.1 MiB of 8.2 MiB… 2 s
```

With `--no-ui`, standard error shows progress on a line that updates in
place instead, and a summary when the transfer is over:

```text
110,101 entries · 76,448 of 100,000 files · 74.7 KiB · 14.9 KiB/s
ferry: 110,101 entries compared, 100,000 files sent, 97.7 KiB in 5.4 s (18.2 KiB/s)
```

The entries counted are those scanned; the files, those whose content is
sent, out of those needed. Once compression saves something, the line says
how much. Scanning is usually faster than sending, so the totals settle early.
A dry run's line, as it may list nothing for a long time, shows how far its
comparison has got instead, and what holds it back: the side whose threads
work the most, and at what. Its listing goes to standard output, written out
as it comes on a terminal, the line cleared before it:

```text
312,450 compared · 12 to send · 1:42 · Source on fatcat: list directories
```

Where standard error is not a terminal, nothing is shown, unless `--progress`
asks for it; then a line is written every few seconds instead of redrawn.

With `--profile`, after the summary comes where the time went: for the
client and each server, each role of their threads on a line, with the
states it spent its time in. It shows which side, and which stage, held the
transfer back. See [profiling](design/profiling.md#the-profile).

Errors and warnings go to standard error, starting with `ferry:`. A path in
SOURCE that cannot be read is reported there, skipped, and counted. One
that vanished between being listed and being read is warned of and left
out, and fails nothing: `sync` deletes what DESTINATION has of it. An entry
of DESTINATION that cannot be compared, written, or finished fails alone,
and the rest is copied; but a DESTINATION drive that is full, out of quota,
or read-only stops the transfer at once, as every entry after would fail
the same way.

On Ctrl-\, `SIGQUIT`, or when nothing has moved for 60 s, no count either
server reports changed, standard error says where every thread of the
client and of each server is, and for how long it has been there; the
transfer goes on. See
[profiling](design/profiling.md#a-snapshot-of-every-thread).

## Exit status

| Status | Means |
|---|---|
| 0 | Every entry of SOURCE was read and compared, and, without `--dry-run`, copied, but for what is warned of, such as devices that need privilege, or an owner that could not be set |
| 1 | The transfer failed, or never began: a host could not be reached, the roots overlap, a drive is full |
| 2 | The command line is wrong: an unknown option, an argument missing, or an option not built yet, as `--fsync` |
| 3 | `ferry update --check`: a newer release is out |
| 23 | Some entries could not be copied or deleted, or some paths in SOURCE could not be read, and the rest were copied, as rsync's partial transfer |
| 24 | Every entry was copied, but some paths in SOURCE vanished as they were read, as rsync's vanished source files |
| 101 | An internal error: a bug in Ferry, said in a line that begins `ferry: internal error` |
| 128 + signal | Interrupted, such as 130 for Ctrl-C |

Exit statuses 2, 3, 23, and 24 are new in this release: before, the last
three were 2, 1, and 1.

## Remote hosts

Ferry runs the system's `ssh`, so everything in `~/.ssh/config` applies:
aliases, `User`, `Port`, `ProxyJump`, identities, agents, `ConnectTimeout`,
which is otherwise 10 s. A password or OTP is asked for in the terminal, as
`ssh` asks for it. Only what would break Ferry's own sessions is left out:
they run no `RemoteCommand` and no `LocalCommand`, and set up no forwarding,
which each of Ferry's masters would otherwise bind again, and hold for as
long as it lasts. Any login shell on the host will do, csh, tcsh, and fish
too: Ferry's commands reach it as one plain line, which it hands to `sh`.

On the host, the client runs a `ferry server` of exactly its own build. The
server speaks to the client over the SSH connection, and exits when the
connection closes. A server of another build refuses the connection and says
which two builds met.

### Installing on a host

When the host does not have the client's build, the client installs it:

1. It probes the host over SSH for its machine ID, OS, architecture, installed
   builds, and `cargo`. Every host of the run is probed, and every one that
   needs it asked about, then installed, before any server starts.
2. On a host where Ferry has never been installed, it asks first. Without a
   terminal to ask on, it refuses unless given `--yes`. A host that already
   has another build is not asked again.
3. A release build downloads its version's binary for the host's platform
   from GitHub Releases with `curl`, checks it against the release's
   `SHA256SUMS`, keeps it in `~/.ferry/releases/<version>/`, and uploads it
   over SSH. The host needs neither network access nor a toolchain. Releases
   are for Linux and macOS on x86_64 and aarch64; on any other host, use
   `--remote-ferry`. The host installs the upload only once its size and
   SHA-256 match, and it runs there: an upload cut short installs nothing,
   and the next run installs it again.
4. A dev build uploads its source and builds it on the host with the host's
   `cargo`, so the host needs a Rust toolchain. Build output is shown as it
   happens. The client refuses if its source has changed since it was built:
   rebuild it, and run again. What the host built is installed only if it is
   the client's build. Two dev installs on one host build one after the
   other.

Only a binary from a GitHub release is a release build. One built from
source, even with `cargo install` or `--release`, is a dev build.

A host keeps the three most recently used builds. Using or installing a
build removes the others, but for any used in the last hour. On a host with
no `find` that tells how long ago a build was used, all but the three are
removed.

Ferry never changes the host's `PATH`, and never runs a `ferry` found there.

### The SSH connection

Each host is reached over one SSH connection, opened first, before anything
runs there, and only logged in. A password or OTP is asked for then, and
never during the transfer.

- **One login per host.** The SSH connection is your own `ControlMaster`,
  if your SSH configuration sets one up for the host (as `ssh -G` shows),
  or else one of Ferry's own. Either way, a later `ssh` to the host, from
  the same run or a run in the next ten minutes, reuses it without logging
  in again. The login is first tried without asking, which works with a
  key, and otherwise asked for in the terminal. Without a terminal, or if
  the login fails, the run stops before anything is copied, saying why, as
  `ssh` said it: that it needs a login only where `ssh` was refused one,
  and otherwise, say, that the host could not be found.
- **Everything goes over it.** The probe, the install, the server, and the
  transfer's one stream all go over the host's one SSH connection, so they
  all reach the same machine, though the host's name may lead to several,
  as a cluster's login nodes behind one name do. `--ssh-connections`, which
  opened several, is gone: a command line that still gives it is refused,
  with exit status 2.
- **A dropped SSH connection fails the transfer**, as rsync's does. The
  stream is told something every 5 seconds when nothing else goes on it,
  so one that goes silent, as when the network goes away or a laptop
  sleeps, is found within 20 seconds, and one that closes, at once; so is a
  server that says nothing on its control connection for 20 seconds. The
  transfer then fails, saying why, such as that the SSH connection the
  server ran over dropped; nothing is logged in again. Files already
  complete stay, and a rerun skips them.

## Cores

On Linux, Ferry runs on at most 8 of each machine's cores, unless `--cpus`
says otherwise: the client's, and each server's on its host, so that a
transfer on a host many share, such as a cluster's login node, leaves the
rest to the others there. The client's process ID picks the cores, for the
client and each server alike, so that several Ferrys on one machine are not
all on the same cores, and they are all on one NUMA node, where one has
enough: spread over two, a copy took half as long again. With 200,000 files
of 4 KiB on 96 cores, kept to 8 a copy was as fast or faster, and spent half
the CPU or less: more cores went mostly on threads waiting for each other in
the kernel. `--cpus all` runs on all of them. On macOS every core is used.

## Files

A release build and a dev build never share any of these: a release build
uses the names below, and a dev build the same names with `-dev`, such as
`~/.ferry-dev/`.

| Where | Path | Holds |
|---|---|---|
| client | `/tmp/ferry-<uid>/ssh/` | the sockets of Ferry's own SSH connections, one per host, mode 0700 |
| client, host | `/tmp/ferry-<uid>/join/`, or on Linux `$XDG_RUNTIME_DIR/ferry/join/` | while a server runs, the socket the stream, and its control connection's channel, join it by, mode 0700 |

Where another user made `/tmp/ferry-<uid>` first, which Ferry would not use,
it uses the first of `/tmp/ferry-<uid>-1` to `-9` that is yours alone, the
same each time.
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
| `FERRY_SSH` | The command to run instead of `ssh`, such as `ssh -p 2222`, as rsync's `RSYNC_RSH`: split at spaces, but for words in quotes, and given `ssh`'s arguments after its own |
| `FERRY_SUDO_PASSWORD` | For `--remote-sudo`: the password `sudo` needs on each host, instead of asking for it. Read once, as Ferry starts, and passed to no program it runs |
| `NO_COLOR` | Set to anything, the dashboard shows no color |

Ferry raises its own limit on open files (`ulimit -n`) to the most the
system allows, the hard limit, on macOS no more than
`kern.maxfilesperproc`, and gives the programs it runs, such as `ssh`, the
limit it started with. Where that is still under 1024 on this machine, it
warns, and works within it, more slowly.

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
| `--check` | Only say whether a newer release is out: exit 3 if one is, 0 if not, 1 if the check failed |
| `--version VERSION` | Update to VERSION instead, such as `0.1.0` |

## `ferry test`

```text
ferry test [--yes] [--remote-ferry PATH] [--remote-join] [--scale SIZE --kind KIND] SOURCE DESTINATION
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
hidden `ferry fixture` subcommand, so a host needs nothing but SSH. `--yes`,
`--remote-ferry`, and `--remote-join` mean what they mean for a transfer,
and apply to every transfer the test runs.

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
side, on a host over SSH or on this machine, and makes the stream between
the two. Its standard input and output are its control connection: it
shakes hands there, and ends once that closes. It then runs its peer over
the stream to the other server, which joins it: `ferry server --join
<token>`, run where SSH will not forward to its socket, hands its own
standard input and output to the server through a Unix socket, and waits
until the server is done. Neither is meant to be run by hand.
