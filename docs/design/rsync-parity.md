# Parity with rsync

[中文](rsync-parity.zh-TW.md)

Where Ferry and rsync do the same thing, Ferry leaves in DESTINATION what
rsync leaves, and exits as rsync does; why, and the differences kept on
purpose, are in the
[overview](overview.md#where-rsync-does-the-same-the-result-is-rsyncs).
This is how that is checked: `tests/rsync.rs` runs both on the same trees,
on every push, and compares what they leave.

**Contents**

- [How a case runs](#how-a-case-runs)
- [What is compared](#what-is-compared)
- [Where it runs](#where-it-runs)
- [The cases](#the-cases)
- [The differences kept on purpose](#the-differences-kept-on-purpose)
- [What it found](#what-it-found)
- [What it does not compare](#what-it-does-not-compare)
- [Not chosen](#not-chosen)

## How a case runs

**Decided** (2026-10-22). Each case is a test of its own, so that they run
in parallel:

1. It builds a SOURCE and a DESTINATION twice, once in a directory for
   rsync and once in one for Ferry, side by side in a temporary directory.
   Every entry is given one mtime, to the nanosecond, unless the case says
   another, and each directory's is set back to it once its entries are
   made.
2. It runs rsync on one and Ferry on the other:
   - `copy` against `rsync -a SOURCE DESTINATION`;
   - `sync` against `rsync -a --delete SOURCE DESTINATION`, rsync also
     given `--stats` and Ferry `--progress`, for how many entries each
     deleted, each run after a dry run of the same, for how many each
     would;
   - an option against rsync's of the same name: `-x`, `--numeric-ids`,
     `--modify-window`.

   Both are given SOURCE and DESTINATION written the same, SOURCE with a
   trailing slash where it is a directory, so that its contents are
   compared, unless the case says otherwise. Each reads its standard input
   from `/dev/null`; Ferry runs with `--no-ui`.
3. A rerun runs each again, after the same change to both.
4. Each case is tried twice: as it is, and with Ferry also given what must
   change nothing it leaves, `--listers 4` and the hidden
   `--batch-entries 2`, so that several directories are listed, and
   compared, at once, each in batches of two names. A small tree so goes
   the ways a large one does.
5. It takes everything under each directory, the roots and what a symlink
   root points to among it, and compares the two, path by path, the exit
   statuses, and, for `sync`, how many entries each deleted. A difference
   fails the test, which lists each path that differs, both statuses, both
   counts, and what each tool said.

## What is compared

| What | How |
|---|---|
| kind | file, directory, symlink, FIFO, socket, device, or other |
| content | a file's bytes, by their hash, and its size |
| permissions | the mode's lower twelve bits, setuid, setgid and sticky among them |
| owner and group | the numeric IDs |
| mtime | to the nanosecond; one set by the run, within the last minute, counts as the same for both, as the two runs set it at different moments |
| symlink target | as it is read |
| link count | of a file |
| device number | of a device |
| exit status | 0, 23, 24, or an error: which error is not compared, rsync's 3, refusing what it was given, being Ferry's 1 |
| entries deleted | for `sync`, each run's, and its dry run's before it: rsync's `Number of deleted files`, from `--stats`, against the `deleted` line of Ferry's summary, and its dry run's `to delete` |

What else each says on the way, messages, warnings and progress, is not
compared: it is each tool's own.

## Where it runs

- **On Linux alone.** macOS ships openrsync, which is not the rsync users
  know.
- **Where rsync is installed.** Without it, each test says it is skipped,
  and passes, unless `FERRY_TEST_RSYNC` is `required`: CI's workflow sets
  it for every job, so that a Linux job without rsync fails rather than
  skipping. The distributions' containers do not pass it on, and skip.
- **As root, for some.** The mount points need root, which a user
  namespace with mounts of its own gives, as `unshare -rm` runs it; owners
  and devices need root of the whole machine. Each says it is skipped
  otherwise. `make root-test` runs them through sudo, with
  `FERRY_TEST_ROOT` `required`, so that one not root fails rather than
  passing unrun; CI's root job runs it. The cases of what cannot be read
  are skipped as root, which reads everything.
- **Between two directories on this machine.** Over SSH the same servers
  run, but no test compares what they leave there.
- **Fast.** The 37 tests took 5.5 s on this machine, each case tried both
  ways, against rsync 3.2.7 (2026-10-30); the 36 before, tried once, took
  2.8 s. The tests do not say which rsync they ran against.

## The cases

| Kind | Test | What |
|---|---|---|
| copying | `an_empty_directory` | an empty SOURCE |
| | `a_tree_of_every_kind` | nested files, an empty file, one of 3 MiB, relative, absolute and dangling symlinks, a FIFO, a Unix socket, an empty directory |
| | `names_of_every_byte` | names with a space, a newline, a tab, a backslash, bytes not UTF-8, a leading `-`, a `*` |
| | `copy_keeps_what_the_destination_has_besides` | `copy` leaves what DESTINATION has and SOURCE has not |
| | `hard_links_are_two_files` | without `-H`, two names of one file are two files |
| attributes | `every_permission` | 600, 755, 444, setuid, setgid; a sticky directory, a setgid one, a read-only one with a file in it |
| | `mtimes_to_the_nanosecond` | of a file, a directory and a symlink |
| | `a_permission_alone_differing` | the same file, another mode |
| reruns | `a_rerun_with_nothing_changed` | |
| | `a_rerun_with_content_changed_its_size_kept` | a new mtime |
| | `a_rerun_with_a_permission_changed` | |
| | `a_rerun_with_an_mtime_changed` | |
| | `a_rerun_with_its_size_changed_its_mtime_kept` | both take it as changed |
| | `content_changed_in_place_its_size_and_mtime_kept` | both take it as unchanged |
| | `mtimes_within_the_modify_window` | five cases: half a second apart, with and without `--modify-window=-1`; a second apart; two and three seconds apart with `--modify-window=2` |
| kinds | `a_directory_where_a_file_is` | |
| | `a_file_where_a_full_directory_is` | `copy` and `sync` |
| | `a_file_where_an_empty_directory_is` | |
| | `a_file_where_a_symlink_is` | |
| | `a_symlink_where_a_file_is` | |
| | `a_directory_where_a_symlink_to_one_is` | the link replaced, never gone through |
| | `a_symlink_where_a_full_directory_is` | `copy` and `sync` |
| deleting | `sync_deletes_what_the_source_has_not` | a file, a tree, a FIFO |
| | `sync_deletes_a_deep_tree` | 150 directories deep |
| | `sync_deletes_in_and_under_read_only_directories` | |
| reading | `an_unreadable_file` | `copy` and `sync`; not as root |
| | `an_unreadable_directory` | `copy` and `sync`; not as root |
| roots | `roots_of_every_kind` | 70: SOURCE a file, a directory, a symlink to a file, to a directory, or to nothing, without a trailing slash, and a directory and a symlink to one with one, onto DESTINATION a file, a directory, a symlink to a file, to a directory, or nothing there, each written without a trailing slash and with one |
| | `a_source_without_a_slash_is_copied_as_itself` | a directory without a trailing slash, by `copy` and by `sync`, into a DESTINATION that has other things, and into none |
| | `a_source_symlink_run_again` | a SOURCE symlink to a file, run again, and run again once it points elsewhere |
| rules | `rules_leave_out_what_rsyncs_do` | eighteen lists of `--exclude`, `--include`, `--exclude-from` and `--include-from`, by `copy` and by `sync`, SOURCE with a trailing slash and without: names at every depth, from the root, a path's end, directories alone, wildcards, the order, a directory not gone into, `+ ` and `- `, a file of rules, and SOURCE itself left out |
| | `delete_excluded_deletes_what_the_rules_leave_out` | `sync --delete-excluded` |
| | `a_directory_holding_what_is_left_out_is_not_replaced` | a file of SOURCE where DESTINATION has a directory holding what is left out |
| as root | `a_mount_point_crossed_or_not` | with and without `-x` |
| | `sync_one_file_system_deletes_nothing_on_another` | `sync -x` |
| | `owners_and_devices` | owners with no name here, setuid, a character device; with and without `--numeric-ids` |

## The differences kept on purpose

The [overview](overview.md#where-rsync-does-the-same-the-result-is-rsyncs)
lists them, with why. Four are checked against rsync, a test running both
and checking that each does what the table says; four are checked on
Ferry's side alone:

| Difference | Checked by | How |
|---|---|---|
| an owner root cannot set | `an_owner_not_set_takes_setuid_and_setgid_away`, in `src/destination/tests.rs` | Ferry alone; what rsync does was found by running it, on 2026-10-22 |
| DESTINATION inside SOURCE | `deliberately_overlapping_roots_are_refused` | against rsync: rsync copies SOURCE into itself, Ferry refuses, and does not exit with 0 |
| `sync` past a directory it could not read | `deliberately_sync_deletes_past_what_it_could_not_read` | against rsync: rsync leaves the rest, Ferry deletes it, keeping what has the name it could not read; the same status; not as root |
| a rule no path can match | `what_cannot_be_done_is_refused_first`, in `tests/rules.rs` | Ferry alone; rsync takes `./build`, and matches nothing with it, found by running it on 2026-11-02 |
| `copy --delete-excluded` | `what_cannot_be_done_is_refused_first`, in `tests/rules.rs` | Ferry alone; rsync deletes, found by running it on 2026-11-02 |
| `sync` into `/` or a home directory | `sync_asks_before_emptying_root_or_a_home`, in `src/prepare/plan.rs`; `syncing_into_a_hosts_home_is_refused_without_a_terminal`, in `tests/remote.rs` | Ferry alone |
| `sync` from a file, or a symlink without a trailing slash | `deliberately_sync_refuses_a_source_that_is_not_a_directory` | against rsync: rsync copies it into DESTINATION and deletes nothing, Ferry refuses, and does not exit with 0 |
| a transfer refused before it starts | `deliberately_a_refusal_exits_with_1` | against rsync: a directory onto a file, both an error, rsync's code 3, Ferry saying the DESTINATION is not a directory |

## What it found

Before the tests, on 2026-10-22, 60 cases were run by hand against rsync
3.2.7, on ext4, as a user, and in `unshare -rm` for those that need root.
50 were the same, and the differences kept on purpose were confirmed; three
were new:

- **A SOURCE that is a symlink to a file.** rsync copies the link; Ferry
  followed it, and copied the file. It now copies the link, as rsync, and
  follows only a link to a directory (`ed7291b`).
- **A path of SOURCE that could not be read.** rsync exits with 23, and
  with 24 only for one that vanished as it was read; Ferry exited with 24.
  It now exits with 23 (`ed7291b`).
- **A transfer refused before it starts.** rsync exits with 3, Ferry with
  1. Kept, as Ferry's 3 is `ferry update --check` finding a newer release.

The tests came after (`5332b2e`), and the one of `--modify-window` as the
draft was merged into the overview (`8c67679`). Why rsync's behaviour is run, not
remembered: on 2026-10-20 it was written down that rsync keeps setuid
where it cannot set the owner, as Ferry then did; run on 2026-10-22, rsync
left the permissions and mtime unset, the file `0700`.

## What it does not compare

- What Ferry leaves on a host, over SSH.
- Owners mapped by name between two machines: here both sides are one
  machine, and the owners of `owners_and_devices` have no name.
- An owner that cannot be set, against rsync; see the table above.
- Exit status 24, for a path that vanishes as it is read.
- What a dry run lists, as nothing said is compared; a dry run's tree is
  not compared either.

## Not chosen

- **A script that runs both,** rather than tests: it would not run with
  the others, nor in CI, and comparing would be by hand again.
- **Trees made at random.** They find cases no one thought of, but a
  failure is hard to read. A version with a fixed seed could come once the
  cases in the table hold.
- **Comparing with macOS's openrsync,** which is not the rsync users know.
