# Ferry CLI

How the `ferry` command behaves today. The [design](design/overview.md)
describes where it is going, and [TODO.md](../TODO.md) the order the rest is
built in. Anything not described here is not built yet.

## Synopsis

```text
ferry copy [OPTIONS] SOURCE DESTINATION
ferry sync [OPTIONS] SOURCE DESTINATION
ferry test [OPTIONS] SOURCE DESTINATION
```

| Command | Makes DESTINATION |
|---|---|
| `copy` | hold everything in SOURCE, keeping anything else it has, as `rsync -a SOURCE/ DESTINATION/` |
| `sync` | the same as SOURCE, deleting anything else it has, as `rsync -a --delete SOURCE/ DESTINATION/` |

Both copy SOURCE's contents into DESTINATION, as if SOURCE ended with a slash,
whether it does or not: from `data` to `backup`, `data/x` becomes `backup/x`,
never `backup/data/x`. rsync's distinction between `data` and `data/` does not
exist in Ferry.

So far Ferry only compares: `copy` runs with `--dry-run` and lists what it
would transfer. Without `--dry-run` it refuses, and copies nothing. `sync`
refuses altogether until deleting is built.

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

SOURCE must be local for now. Copying from a host is not built yet.

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

| SOURCE has | Unchanged when DESTINATION has |
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

A directory's contents are compared entry by entry, so an unchanged directory
can still have changed entries listed under it. Permissions, owners, and
directory times are not compared yet.

## Options

`copy` and `sync` take the same options.

| Option | Effect |
|---|---|
| `-n`, `--dry-run` | List what would be transferred, without copying anything. Required for now |
| `--modify-window SECONDS` | Count mtimes up to `SECONDS` apart as the same. `0`, the default, compares whole seconds; `2` suits FAT; `-1` compares nanoseconds too. As rsync's option of the same name |
| `-y`, `--yes` | Install Ferry on a host without asking |
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

An unchanged tree prints nothing.

Errors and warnings go to standard error, starting with `ferry:`. A path in
SOURCE that cannot be read is reported there, skipped, and counted.

## Exit status

| Status | Means |
|---|---|
| 0 | Every entry of SOURCE was read and compared |
| 1 | Something failed, or some paths in SOURCE could not be read |

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
3. A dev build uploads its source and builds it on the host with the host's
   `cargo`, so the host needs a Rust toolchain. Build output is shown as it
   happens. The client refuses if its source has changed since it was built:
   rebuild it, and run again.

Installing a release build is not built yet; a release client that finds no
matching build on a host asks for `--remote-ferry`.

Ferry never changes the host's `PATH`, and never runs a `ferry` found there.

### One login per host

The client shares one authenticated SSH connection per host
(`ControlMaster`). The first `ssh` logs in, and every later one, from the same
run or a run in the next ten minutes, reuses it without logging in again. To
leave `ControlMaster` to your own SSH configuration instead, turn this off in
the configuration file.

## Configuration

`config.toml` in Ferry's directory on the client. Every setting is optional,
and an unknown setting is an error.

```toml
[ssh]
# Share one authenticated connection per host. Default: true.
manage_control_master = true
```

## Files

A release build and a dev build never share any of these: a release build
uses the names below, and a dev build the same names with `-dev`, such as
`~/.ferry-dev/`.

| Where | Path | Holds |
|---|---|---|
| client | `~/.ferry/config.toml` | the configuration |
| client | `/tmp/ferry-<uid>/ssh/` | the shared SSH connections' sockets, mode 0700 |
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

## `ferry test`

```text
ferry test [--yes] [--remote-ferry PATH] SOURCE DESTINATION
```

Runs the whole flow from SOURCE to DESTINATION on trees it creates, and
removes them afterwards. SOURCE and DESTINATION are directories, local or
`[user@]host:path`, and may be the same directory. Inside them the test
creates `ferry-test-<id>-source` and `ferry-test-<id>-destination`, and
removes only those: removal refuses a directory without the `.ferry-fixture`
marker that every test tree has.

```text
$ ferry test /tmp workbox:/tmp
ferry test: from /tmp/ferry-test-82812cf67b98-source to workbox:/tmp/ferry-test-82812cf67b98-destination
  ok    a missing destination needs everything (0.12s)
  ok    an identical destination needs nothing (0.13s)
  ok    a changed destination needs what changed (0.10s)
  ok    mtimes in the same second are equal (0.09s)
  ok    --modify-window=-1 compares nanoseconds (0.13s)
5 passed, 0 failed, 0 skipped
```

The source tree holds a file, nested directories, an empty directory, a
symlink, a dangling symlink, and a FIFO, with fixed contents and mtimes down
to the nanosecond. Each scenario puts a known tree at the destination, runs
the real `ferry` command between the two places, and checks what it prints.
A scenario that needs something not built yet, such as copying from a host,
is skipped and says why. The scenarios grow as Ferry does.

On a host, the test trees are created and removed by Ferry itself, through a
hidden `ferry fixture` subcommand, so a host needs nothing but SSH. Ferry is
installed on each host before the first scenario, as for a transfer; `--yes`
and `--remote-ferry` mean what they mean there.

The exit status is 0 when no scenario failed, and 1 otherwise.

## `ferry server`

A hidden subcommand: the server side of a connection, speaking packets on
standard input and output. The client starts it, over SSH or as a child
process for a local DESTINATION. It is not meant to be run by hand.
