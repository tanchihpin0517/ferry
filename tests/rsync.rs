//! What Ferry does, against what rsync does, on the same trees: where the two
//! overlap, users coming from rsync expect the same result. Each test builds
//! a SOURCE and a DESTINATION twice, runs `rsync -a` (with `--delete` for
//! `sync`) on one and Ferry on the other, and compares everything under
//! both: each path's kind, content, permissions, mtime, symlink target, owner
//! and link count; and the exit statuses, as 0, 23, 24 or an error. Each is
//! tried again with Ferry given what must change nothing it leaves: several
//! directories listed, and compared, at once, each in batches of two names.
//!
//! Where Ferry differs on purpose, a test says so, and checks that it does.
//! Why each difference is kept is in docs/design/overview.md, under "Where
//! rsync does the same, the result is rsync's"; what these tests cover, and
//! what they do not, in docs/design/rsync-parity.md.
//!
//! Linux only: macOS ships openrsync, which is not the rsync users know.
//! Without rsync each test says it is skipped, and passes, unless
//! `FERRY_TEST_RSYNC` is `required`, as CI's Linux jobs set it. Those that
//! need root say so, and pass, unless run as root, or unless
//! `FERRY_TEST_ROOT` is `required`, as `make root-test` sets it.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, FileType, Mode, Timespec, Timestamps};

/// The mtime every entry is given, in nanoseconds, unless it says another.
const T0: i64 = 1_700_000_000_123_456_789;

/// Whether rsync is here to compare with. Says the test is skipped if not;
/// fails if it must be.
fn rsync() -> bool {
    let there = Command::new("rsync")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !there {
        assert!(
            std::env::var("FERRY_TEST_RSYNC").as_deref() != Ok("required"),
            "rsync is required, and not installed"
        );
        eprintln!("skipped: needs rsync");
    }
    there
}

/// Whether this test can mount: as root, if only of a user namespace with
/// its own mounts, as `unshare -rm` runs it. Says it is skipped if not.
fn can_mount() -> bool {
    let root = rustix::process::geteuid().is_root();
    if !root {
        skip_root();
    }
    root
}

/// Whether this test can run: as root, of the whole machine, so that other
/// owners can be given. Says it is skipped if not.
fn root() -> bool {
    let whole = fs::read_to_string("/proc/self/uid_map")
        .is_ok_and(|map| map.split_whitespace().collect::<Vec<_>>() == ["0", "0", "4294967295"]);
    let root = rustix::process::geteuid().is_root() && whole;
    if !root {
        skip_root();
    }
    root
}

/// Says a test that needs root is skipped; fails if it must run.
fn skip_root() {
    assert!(
        std::env::var("FERRY_TEST_ROOT").as_deref() != Ok("required"),
        "root is required, and this is not it"
    );
    eprintln!("skipped: needs root");
}

fn set_mtime(path: &Path, ns: i64) {
    let at = Timespec {
        tv_sec: ns / 1_000_000_000,
        tv_nsec: (ns % 1_000_000_000) as _,
    };
    let times = Timestamps {
        last_access: at,
        last_modification: at,
    };
    rustix::fs::utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW).unwrap();
}

fn write_at(path: &Path, content: &[u8], mode: u32, ns: i64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    set_mtime(path, ns);
}

fn write(path: impl AsRef<Path>, content: &[u8]) {
    write_at(path.as_ref(), content, 0o644, T0);
}

fn dir_at(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn dir(path: impl AsRef<Path>) {
    dir_at(path.as_ref(), 0o755);
}

fn link(target: &str, path: impl AsRef<Path>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(target, path).unwrap();
    set_mtime(path, T0);
}

fn fifo(path: impl AsRef<Path>) {
    let path = path.as_ref();
    rustix::fs::mknodat(CWD, path, FileType::Fifo, Mode::from_raw_mode(0o644), 0).unwrap();
    set_mtime(path, T0);
}

/// A Unix socket's name, as `mknod` makes one, which nothing listens on.
fn socket(path: impl AsRef<Path>) {
    let path = path.as_ref();
    rustix::fs::mknodat(CWD, path, FileType::Socket, Mode::from_raw_mode(0o755), 0).unwrap();
    set_mtime(path, T0);
}

/// Each directory's mtime, which making its entries changed, set back to
/// [`T0`], deepest first; those it may not change are left.
fn settle(root: &Path) {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries {
            settle(&entry.unwrap().path());
        }
    }
    let _ = rustix::fs::utimensat(
        CWD,
        root,
        &Timestamps {
            last_access: Timespec {
                tv_sec: T0 / 1_000_000_000,
                tv_nsec: (T0 % 1_000_000_000) as _,
            },
            last_modification: Timespec {
                tv_sec: T0 / 1_000_000_000,
                tv_nsec: (T0 % 1_000_000_000) as _,
            },
        },
        AtFlags::SYMLINK_NOFOLLOW,
    );
}

/// Every directory under `root` made searchable and writable again, so the
/// tree can be removed.
fn unlock(root: &Path) {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return;
    };
    if metadata.is_dir() {
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries {
                unlock(&entry.unwrap().path());
            }
        }
    }
}

/// When an mtime was set by a run, rather than given: the two runs set it
/// at different moments, which is no difference.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mtime {
    At(i64),
    Now,
}

#[derive(Debug, Clone, PartialEq)]
struct Found {
    kind: &'static str,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: Mtime,
    size: Option<u64>,
    nlink: Option<u64>,
    content: Option<u64>,
    target: Option<PathBuf>,
    rdev: Option<u64>,
}

/// Everything under `root`, by path from it, root and all.
fn snapshot(root: &Path, ran_from: i64) -> BTreeMap<Vec<u8>, Found> {
    let mut found = BTreeMap::new();
    let mut waiting = vec![(root.to_path_buf(), Vec::new())];
    while let Some((path, name)) = waiting.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        let mtime = metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec();
        let mut entry = Found {
            kind: "other",
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime: if mtime >= ran_from {
                Mtime::Now
            } else {
                Mtime::At(mtime)
            },
            size: None,
            nlink: None,
            content: None,
            target: None,
            rdev: None,
        };
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_symlink() {
            entry.kind = "symlink";
            entry.target = Some(fs::read_link(&path).unwrap());
        } else if file_type.is_file() {
            entry.kind = "file";
            entry.size = Some(metadata.len());
            entry.nlink = Some(metadata.nlink());
            entry.content = fs::read(&path).ok().map(|bytes| {
                let mut hasher = DefaultHasher::new();
                hasher.write(&bytes);
                hasher.finish()
            });
        } else if file_type.is_dir() {
            entry.kind = "directory";
            if let Ok(entries) = fs::read_dir(&path) {
                for child in entries {
                    let child = child.unwrap();
                    let mut child_name = name.clone();
                    if !child_name.is_empty() {
                        child_name.push(b'/');
                    }
                    child_name.extend_from_slice(child.file_name().as_bytes());
                    waiting.push((child.path(), child_name));
                }
            }
        } else if file_type.is_fifo() {
            entry.kind = "fifo";
        } else if file_type.is_socket() {
            entry.kind = "socket";
        } else if file_type.is_char_device() || file_type.is_block_device() {
            entry.kind = "device";
            entry.rdev = Some(metadata.rdev());
        }
        found.insert(name, entry);
    }
    found
}

/// What one run of a case did.
struct Ran {
    /// Each run's exit status, as 0, 23, 24, or an error.
    statuses: Vec<String>,
    said: String,
    tree: BTreeMap<Vec<u8>, Found>,
}

/// A transfer to try both ways.
#[derive(Clone, Copy)]
struct Case {
    sync: bool,
    args: &'static [&'static str],
    source: &'static str,
    destination: &'static str,
    /// Whether rsync is given SOURCE with a slash: if not said, when it is a
    /// directory, as Ferry takes every SOURCE.
    slash: Option<bool>,
    /// Options Ferry alone is given, which must change nothing it leaves.
    ferry: &'static [&'static str],
}

const COPY: Case = Case {
    sync: false,
    args: &[],
    source: "source",
    destination: "destination",
    slash: None,
    ferry: &[],
};

const SYNC: Case = Case { sync: true, ..COPY };

/// Several directories listed, and compared, at once, each a batch of two
/// names at a time: what Ferry leaves must be the same.
const SPREAD: &[&str] = &["--listers", "4", "--batch-entries", "2"];

/// The case as it is, and with Ferry given [`SPREAD`].
fn ways(case: Case) -> [Case; 2] {
    [
        case,
        Case {
            ferry: SPREAD,
            ..case
        },
    ]
}

type Setup = fn(&Path, &Path);

fn class(code: Option<i32>) -> String {
    match code {
        Some(0) => "0".into(),
        Some(23) => "23".into(),
        Some(24) => "24".into(),
        // Which error is not compared: rsync's 3, refusing its arguments,
        // is Ferry's 1, its 3 being `ferry update --check`'s.
        _ => "an error".into(),
    }
}

/// Builds the trees in `work`, and runs `tool` on them, once, or twice with
/// `then` between.
fn run(tool: &str, work: &Path, case: Case, setup: Setup, then: Option<Setup>) -> Ran {
    fs::create_dir_all(work).unwrap();
    let (source, destination) = (work.join(case.source), work.join(case.destination));
    setup(&source, &destination);
    settle(work);
    let mut statuses = Vec::new();
    let mut said = String::new();
    let runs = if then.is_some() { 2 } else { 1 };
    for attempt in 0..runs {
        if attempt == 1 {
            then.unwrap()(&source, &destination);
        }
        let mut command;
        if tool == "rsync" {
            command = Command::new("rsync");
            command.arg("-a");
            if case.sync {
                command.arg("--delete");
            }
            command.args(case.args);
            let slash = case.slash.unwrap_or_else(|| source.is_dir());
            let mut named = source.clone().into_os_string();
            if slash {
                named.push("/");
            }
            command.arg(named);
        } else {
            command = Command::new(env!("CARGO_BIN_EXE_ferry"));
            command.arg(if case.sync { "sync" } else { "copy" });
            command.arg("--no-ui");
            command.args(case.args);
            command.args(case.ferry);
            command.arg(&source);
        }
        let output = command
            .arg(work.join(case.destination).as_os_str())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        statuses.push(class(output.status.code()));
        said.push_str(&String::from_utf8_lossy(&output.stdout));
        said.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ran {
        statuses,
        said,
        tree: BTreeMap::new(),
    }
}

/// What each tool made of the case.
struct Both {
    rsync: Ran,
    ferry: Ran,
    /// What Ferry alone was given.
    given: &'static [&'static str],
    _temp: Temp,
}

/// A temporary directory removed however locked its tree was left, and
/// whatever a setup mounted in it unmounted first.
struct Temp(tempfile::TempDir);

impl Drop for Temp {
    fn drop(&mut self) {
        MOUNTS.with(|mounts| mounts.borrow_mut().clear());
        unlock(self.0.path());
    }
}

fn both(case: Case, setup: Setup, then: Option<Setup>) -> Both {
    let temp = Temp(tempfile::tempdir().unwrap());
    let ran_from = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
        - 60_000_000_000;
    let [rsync, ferry] = ["rsync", "ferry"].map(|tool| {
        let work = temp.0.path().join(tool);
        let mut ran = run(tool, &work, case, setup, then);
        // The whole work directory: what a symlink root points to is in it.
        ran.tree = snapshot(&work, ran_from);
        ran
    });
    Both {
        rsync,
        ferry,
        given: case.ferry,
        _temp: temp,
    }
}

impl Both {
    /// Where the two trees differ, one line a path, and the statuses if they
    /// do.
    fn differences(&self) -> Vec<String> {
        let (r, f) = (&self.rsync.tree, &self.ferry.tree);
        let mut lines: Vec<String> = r
            .keys()
            .chain(f.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .filter(|path| r.get(*path) != f.get(*path))
            .map(|path| {
                format!(
                    "{}: rsync {:?}, ferry {:?}",
                    String::from_utf8_lossy(path),
                    r.get(path),
                    f.get(path)
                )
            })
            .collect();
        if self.rsync.statuses != self.ferry.statuses {
            lines.push(format!(
                "exit: rsync {:?}, ferry {:?}",
                self.rsync.statuses, self.ferry.statuses
            ));
        }
        lines
    }

    fn report(&self) -> String {
        format!(
            "ferry given {:?}\n{}\nrsync said:\n{}\nferry said:\n{}",
            self.given,
            self.differences().join("\n"),
            self.rsync.said,
            self.ferry.said
        )
    }

    fn ferry(&self, path: &str) -> Option<&Found> {
        self.ferry.tree.get(path.as_bytes())
    }

    fn rsync(&self, path: &str) -> Option<&Found> {
        self.rsync.tree.get(path.as_bytes())
    }
}

/// Runs the case both ways, each of its [`ways`], and checks that they come
/// out the same.
fn same(case: Case, setup: Setup) {
    same_after(case, setup, None);
}

fn same_after(case: Case, setup: Setup, then: Option<Setup>) {
    if !rsync() {
        return;
    }
    for case in ways(case) {
        let both = both(case, setup, then);
        assert!(both.differences().is_empty(), "{}", both.report());
    }
}

// ---------------------------------------------------------------- copying

#[test]
fn an_empty_directory() {
    same(COPY, |source, _| dir(source));
}

#[test]
fn a_tree_of_every_kind() {
    same(COPY, |source, _| {
        write(source.join("a/b/c.txt"), b"hello");
        write(source.join("empty"), b"");
        write(source.join("big"), &[7; 3 << 20]);
        link("a/b/c.txt", source.join("relative"));
        link("/etc/hostname", source.join("absolute"));
        link("nowhere", source.join("dangling"));
        fifo(source.join("fifo"));
        socket(source.join("socket"));
        dir(source.join("empty-directory"));
    });
}

#[test]
fn names_of_every_byte() {
    same(COPY, |source, _| {
        for name in [
            &b"with space"[..],
            b"new\nline",
            b"tab\t",
            b"back\\slash",
            b"\xff\xfe-not-utf-8",
            b"-dash",
            b"star*",
        ] {
            write(source.join(OsStr::from_bytes(name)), name);
        }
    });
}

#[test]
fn copy_keeps_what_the_destination_has_besides() {
    same(COPY, |source, destination| {
        write(source.join("a"), b"a");
        write(destination.join("extra"), b"extra");
        write(destination.join("sub/extra"), b"extra");
    });
}

#[test]
fn hard_links_are_two_files() {
    same(COPY, |source, _| {
        write(source.join("a"), b"shared");
        fs::hard_link(source.join("a"), source.join("b")).unwrap();
    });
}

// ---------------------------------------------------------------- attributes

#[test]
fn every_permission() {
    same(COPY, |source, _| {
        for (name, mode) in [
            ("600", 0o600),
            ("755", 0o755),
            ("444", 0o444),
            ("setuid", 0o4755),
            ("setgid", 0o2755),
        ] {
            write_at(&source.join(name), b"x", mode, T0);
        }
        dir_at(&source.join("sticky"), 0o1777);
        dir_at(&source.join("setgid-directory"), 0o2775);
        write(source.join("read-only/inside"), b"in");
        dir_at(&source.join("read-only"), 0o555);
    });
}

#[test]
fn mtimes_to_the_nanosecond() {
    same(COPY, |source, _| {
        write_at(&source.join("f"), b"x", 0o644, 1_600_000_000_987_654_321);
        write_at(&source.join("d/g"), b"x", 0o644, 1_500_000_000_000_000_001);
        link("f", source.join("l"));
        set_mtime(&source.join("l"), 1_400_000_000_111_111_111);
    });
}

#[test]
fn a_permission_alone_differing() {
    same(COPY, |source, destination| {
        write_at(&source.join("f"), b"same", 0o640, T0);
        write_at(&destination.join("f"), b"same", 0o604, T0);
    });
}

// ---------------------------------------------------------------- reruns

#[test]
fn a_rerun_with_nothing_changed() {
    same_after(
        COPY,
        |source, _| {
            write(source.join("f"), b"AAAA");
            write(source.join("d/g"), b"g");
        },
        Some(|_, _| {}),
    );
}

#[test]
fn a_rerun_with_content_changed_its_size_kept() {
    same_after(
        COPY,
        |source, _| write(source.join("f"), b"AAAA"),
        Some(|source, _| write_at(&source.join("f"), b"BBBB", 0o644, T0 + 5_000_000_000)),
    );
}

#[test]
fn a_rerun_with_a_permission_changed() {
    same_after(
        COPY,
        |source, _| write(source.join("f"), b"AAAA"),
        Some(|source, _| {
            fs::set_permissions(source.join("f"), fs::Permissions::from_mode(0o600)).unwrap()
        }),
    );
}

#[test]
fn a_rerun_with_an_mtime_changed() {
    same_after(
        COPY,
        |source, _| write(source.join("f"), b"AAAA"),
        Some(|source, _| set_mtime(&source.join("f"), T0 + 7_000_000_000)),
    );
}

/// Another size, the same mtime: both take it as changed.
#[test]
fn a_rerun_with_its_size_changed_its_mtime_kept() {
    same_after(
        COPY,
        |source, _| write(source.join("f"), b"AAAA"),
        Some(|source, _| write(source.join("f"), b"BBBBBBBB")),
    );
}

/// The same size and mtime, other content: both take it as unchanged.
#[test]
fn content_changed_in_place_its_size_and_mtime_kept() {
    same(COPY, |source, destination| {
        write(source.join("f"), b"NEW!");
        write(destination.join("f"), b"old.");
    });
}

/// `--modify-window`: mtimes no further apart than it are the same, so a
/// file of the same size is taken as unchanged; the default, 0, compares
/// whole seconds, and `-1` nanoseconds too.
#[test]
fn mtimes_within_the_modify_window() {
    if !rsync() {
        return;
    }
    const S: i64 = 1_000_000_000;
    // Each: the window given, the two mtimes, and whether the file is sent.
    let cases: [(&'static [&'static str], Setup, bool); 5] = [
        (&[], |s, d| apart(s, d, S / 2), false),
        (&["--modify-window=-1"], |s, d| apart(s, d, S / 2), true),
        (&[], |s, d| apart(s, d, S), true),
        (&["--modify-window=2"], |s, d| apart(s, d, 2 * S), false),
        (&["--modify-window=2"], |s, d| apart(s, d, 3 * S), true),
    ];
    for (args, setup, sent) in cases {
        for case in ways(Case { args, ..COPY }) {
            let both = both(case, setup, None);
            assert!(both.differences().is_empty(), "{args:?}: {}", both.report());
            let content = |found: Option<&Found>| found.and_then(|found| found.content);
            assert_eq!(
                content(both.rsync("destination/f")) == content(both.rsync("source/f")),
                sent,
                "{args:?}: {}",
                both.report()
            );
        }
    }
}

/// A file of the same size, other content, its mtime `ns` past the
/// Source's, in the same second for less than one.
fn apart(source: &Path, destination: &Path, ns: i64) {
    write_at(&source.join("f"), b"NEW!", 0o644, T0);
    write_at(&destination.join("f"), b"old.", 0o644, T0 + ns);
}

// ---------------------------------------------------------------- kinds

#[test]
fn a_directory_where_a_file_is() {
    same(COPY, |source, destination| {
        write(source.join("a/inner"), b"i");
        write(destination.join("a"), b"file");
    });
}

#[test]
fn a_file_where_a_full_directory_is() {
    for case in [COPY, SYNC] {
        same(case, |source, destination| {
            write(source.join("a"), b"file");
            write(destination.join("a/inner"), b"i");
        });
    }
}

#[test]
fn a_file_where_an_empty_directory_is() {
    same(COPY, |source, destination| {
        write(source.join("a"), b"file");
        dir(destination.join("a"));
    });
}

#[test]
fn a_file_where_a_symlink_is() {
    same(COPY, |source, destination| {
        write(source.join("a"), b"file");
        link("elsewhere", destination.join("a"));
    });
}

#[test]
fn a_symlink_where_a_file_is() {
    same(COPY, |source, destination| {
        link("target", source.join("a"));
        write(destination.join("a"), b"file");
    });
}

/// The link is replaced, never gone through.
#[test]
fn a_directory_where_a_symlink_to_one_is() {
    same(COPY, |source, destination| {
        write(source.join("a/inner"), b"i");
        write(destination.join("../outside/kept"), b"k");
        link("../outside", destination.join("a"));
    });
}

#[test]
fn a_symlink_where_a_full_directory_is() {
    for case in [COPY, SYNC] {
        same(case, |source, destination| {
            link("x", source.join("a"));
            write(destination.join("a/inner"), b"i");
        });
    }
}

// ---------------------------------------------------------------- deleting

#[test]
fn sync_deletes_what_the_source_has_not() {
    same(SYNC, |source, destination| {
        write(source.join("keep"), b"k");
        write(destination.join("keep"), b"k");
        write(destination.join("extra"), b"e");
        write(destination.join("tree/a/b/c"), b"c");
        fifo(destination.join("fifo"));
    });
}

#[test]
fn sync_deletes_a_deep_tree() {
    same(SYNC, |source, destination| {
        dir(source);
        let mut deep = destination.join("deep");
        for _ in 0..150 {
            deep.push("d");
        }
        write(deep.join("leaf"), b"l");
    });
}

#[test]
fn sync_deletes_in_and_under_read_only_directories() {
    same(SYNC, |source, destination| {
        write(source.join("read-only/keep"), b"k");
        dir_at(&source.join("read-only"), 0o555);
        write(destination.join("read-only/keep"), b"k");
        write(destination.join("read-only/extra"), b"e");
        dir_at(&destination.join("read-only"), 0o555);
        write(destination.join("gone/inner"), b"i");
        dir_at(&destination.join("gone"), 0o555);
    });
}

// ---------------------------------------------------------------- reading

/// What a user cannot read: as root, everything can be, so these are a
/// user's.
fn unreadable(setup: Setup, case: Case) {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root reads everything");
        return;
    }
    same(case, setup);
}

#[test]
fn an_unreadable_file() {
    for case in [COPY, SYNC] {
        unreadable(
            |source, destination| {
                write(source.join("ok"), b"ok");
                write_at(&source.join("secret"), b"s", 0o000, T0);
                write(destination.join("secret"), b"old");
            },
            case,
        );
    }
}

#[test]
fn an_unreadable_directory() {
    for case in [COPY, SYNC] {
        unreadable(
            |source, destination| {
                write(source.join("ok"), b"ok");
                write(source.join("locked/inner"), b"i");
                dir_at(&source.join("locked"), 0o000);
                write(destination.join("locked/old"), b"o");
                write(destination.join("extra"), b"e");
            },
            case,
        );
    }
}

// ---------------------------------------------------------------- roots

#[derive(Clone, Copy, Debug)]
enum Root {
    File,
    Directory,
    LinkToFile,
    LinkToDirectory,
    Dangling,
    Missing,
}

fn make_root(path: &Path, root: Root, content: &[u8]) {
    let target = format!("{}-target", path.file_name().unwrap().to_str().unwrap());
    let beside = path.with_file_name(&target);
    match root {
        Root::File => write(path, content),
        Root::Directory => write(path.join("inner"), content),
        Root::LinkToFile => {
            write(&beside, content);
            link(&target, path);
        }
        Root::LinkToDirectory => {
            write(beside.join("inner"), content);
            link(&target, path);
        }
        Root::Dangling => link("nowhere", path),
        Root::Missing => {}
    }
}

/// Every kind of SOURCE onto every kind of DESTINATION.
#[test]
fn roots_of_every_kind() {
    use Root::*;
    for source in [File, Directory, LinkToFile, LinkToDirectory, Dangling] {
        for destination in [File, Directory, LinkToFile, LinkToDirectory, Missing] {
            if !rsync() {
                return;
            }
            // A closure that captures cannot be a `Setup`: the kinds go
            // through the environment of this thread instead.
            KINDS.with(|kinds| kinds.set((source, destination)));
            for case in ways(COPY) {
                let both = both(
                    case,
                    |s, d| {
                        let (source, destination) = KINDS.with(|kinds| kinds.get());
                        make_root(s, source, b"source");
                        make_root(d, destination, b"destination");
                    },
                    None,
                );
                assert!(
                    both.differences().is_empty(),
                    "{source:?} onto {destination:?}:\n{}",
                    both.report()
                );
            }
        }
    }
}

thread_local! {
    static KINDS: std::cell::Cell<(Root, Root)> = const { std::cell::Cell::new((Root::File, Root::File)) };
}

/// A SOURCE that is a symlink to a file is copied as one; a second run, and
/// one after it points elsewhere, as rsync's.
#[test]
fn a_source_symlink_run_again() {
    let setup: Setup = |source, _| make_root(source, Root::LinkToFile, b"source");
    same_after(COPY, setup, Some(|_, _| {}));
    same_after(
        COPY,
        setup,
        Some(|source, _| {
            fs::remove_file(source).unwrap();
            link("other", source);
        }),
    );
}

// ---------------------------------------------------------------- deliberate

/// SOURCE is taken as if it ended with a slash, whether it does or not:
/// rsync, given `source`, makes `destination/source`.
#[test]
fn deliberately_a_source_is_its_contents_without_a_slash() {
    if !rsync() {
        return;
    }
    for case in ways(Case {
        slash: Some(false),
        ..COPY
    }) {
        let both = both(case, |source, _| write(source.join("a"), b"a"), None);
        assert!(
            both.rsync("destination/source/a").is_some(),
            "{}",
            both.report()
        );
        assert!(both.ferry("destination/a").is_some(), "{}", both.report());
        assert!(
            both.ferry("destination/source").is_none(),
            "{}",
            both.report()
        );
    }
}

/// A trailing slash on DESTINATION means nothing: a file onto `new/` is the
/// file `new`, where rsync makes a directory `new` and puts it in.
#[test]
fn deliberately_a_slash_on_a_missing_destination_means_nothing() {
    if !rsync() {
        return;
    }
    for case in ways(Case {
        destination: "new/",
        ..COPY
    }) {
        let both = both(case, |source, _| write(source, b"f"), None);
        assert_eq!(both.rsync("new").map(|found| found.kind), Some("directory"));
        assert_eq!(both.ferry("new").map(|found| found.kind), Some("file"));
    }
}

/// Overlapping roots are refused, where rsync copies SOURCE into itself.
#[test]
fn deliberately_overlapping_roots_are_refused() {
    if !rsync() {
        return;
    }
    for case in ways(Case {
        destination: "source/sub",
        ..COPY
    }) {
        let both = both(case, |source, _| write(source.join("a"), b"a"), None);
        assert!(both.rsync("source/sub/a").is_some(), "{}", both.report());
        assert!(both.ferry("source/sub").is_none(), "{}", both.report());
        assert_ne!(both.ferry.statuses, ["0"]);
    }
}

/// After a directory of SOURCE could not be read, rsync deletes nothing
/// more; Ferry keeps only what is named as what it could not read, and
/// deletes the rest.
#[test]
fn deliberately_sync_deletes_past_what_it_could_not_read() {
    if rustix::process::geteuid().is_root() || !rsync() {
        return;
    }
    for case in ways(SYNC) {
        let both = both(
            case,
            |source, destination| {
                write(source.join("locked/inner"), b"i");
                dir_at(&source.join("locked"), 0o000);
                write(source.join("past/keep"), b"k");
                write(destination.join("past/keep"), b"k");
                write(destination.join("past/extra"), b"e");
                write(destination.join("locked/old"), b"o");
            },
            None,
        );
        assert!(
            both.rsync("destination/past/extra").is_some(),
            "{}",
            both.report()
        );
        assert!(
            both.ferry("destination/past/extra").is_none(),
            "{}",
            both.report()
        );
        assert!(
            both.ferry("destination/locked").is_some(),
            "{}",
            both.report()
        );
        assert_eq!(both.ferry.statuses, both.rsync.statuses);
    }
}

// ---------------------------------------------------------------- as root

/// A tmpfs mounted at `path` until dropped.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

fn mount_tmpfs(path: &Path) -> Mounted {
    fs::create_dir_all(path).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(path)
        .status()
        .unwrap();
    assert!(mounted.success(), "mount -t tmpfs");
    Mounted(path.to_path_buf())
}

thread_local! {
    static MOUNTS: std::cell::RefCell<Vec<Mounted>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[test]
fn a_mount_point_crossed_or_not() {
    if !can_mount() {
        return;
    }
    for args in [&[][..], &["-x"][..]] {
        same(Case { args, ..COPY }, |source, _| {
            write(source.join("top"), b"t");
            let mounted = mount_tmpfs(&source.join("mount"));
            write(source.join("mount/inside"), b"i");
            MOUNTS.with(|mounts| mounts.borrow_mut().push(mounted));
        });
    }
}

#[test]
fn sync_one_file_system_deletes_nothing_on_another() {
    if !can_mount() {
        return;
    }
    same(
        Case {
            args: &["-x"],
            ..SYNC
        },
        |source, destination| {
            write(source.join("top"), b"t");
            let mounted = mount_tmpfs(&destination.join("mount"));
            write(destination.join("mount/extra"), b"e");
            write(destination.join("extra"), b"e");
            MOUNTS.with(|mounts| mounts.borrow_mut().push(mounted));
        },
    );
}

/// A transfer refused before it starts, as a directory onto a file, exits
/// with 1, where rsync's exits with 3, which Ferry's `update --check` has.
#[test]
fn deliberately_a_refusal_exits_with_1() {
    if !rsync() {
        return;
    }
    for case in ways(COPY) {
        let both = both(
            case,
            |source, destination| {
                write(source.join("a"), b"a");
                write(destination, b"file");
            },
            None,
        );
        assert!(both.differences().is_empty(), "{}", both.report());
        assert!(both.rsync.said.contains("(code 3)"), "{}", both.report());
        assert!(
            both.ferry.said.contains("is not a directory"),
            "{}",
            both.report()
        );
    }
}

/// Owners with no name here, kept by number either way, setuid and all; and
/// devices.
#[test]
fn owners_and_devices() {
    if !root() {
        return;
    }
    for args in [&[][..], &["--numeric-ids"][..]] {
        same(Case { args, ..COPY }, |source, _| {
            write_at(&source.join("theirs"), b"t", 0o4755, T0);
            rustix::fs::chownat(
                CWD,
                source.join("theirs"),
                Some(rustix::fs::Uid::from_raw(4001)),
                Some(rustix::fs::Gid::from_raw(4002)),
                AtFlags::empty(),
            )
            .unwrap();
            set_mtime(&source.join("theirs"), T0);
            let null = source.join("null");
            rustix::fs::mknodat(
                CWD,
                &null,
                FileType::CharacterDevice,
                Mode::from_raw_mode(0o666),
                rustix::fs::makedev(1, 3),
            )
            .unwrap();
            set_mtime(&null, T0);
        });
    }
}
