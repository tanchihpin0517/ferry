use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use ferry::destination::{self, Options};
use ferry::frame;
use ferry::manifest::{Entry, Kind, Manifest, Timestamp};
use ferry::protocol::{self, Control};
use ferry::record::{self, Header, Trailer};

fn entry(id: u64, name: &[u8], kind: Kind, size: u64, nanos: u32, mode: u32) -> Entry {
    Entry {
        id,
        name: name.to_vec(),
        kind,
        size,
        mtime: Timestamp { seconds: 1_600_000_000, nanoseconds: nanos },
        mode,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

fn opts(root: &Path, delete: bool) -> Options {
    Options {
        root: root.as_os_str().as_bytes().to_vec(),
        dry_run: false,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete,
        report: false,
    }
}

fn list(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// n complete files, then (optionally) one cut record; then the stream ends.
fn e_cut(n: usize, cut: bool, error_packet: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("dst");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &Manifest::Root(entry(0, b"", Kind::Directory, 0, 0, 0o755))).unwrap();
    let mut entries = Vec::new();
    for i in 0..n {
        entries.push(entry(i as u64 + 1, format!("f{i:03}").as_bytes(), Kind::File, 5, 0, 0o644));
    }
    entries.push(entry(n as u64 + 1, b"zbig", Kind::File, 1_000_000, 0, 0o644));
    protocol::send(&mut packets, &Manifest::Directory { path: vec![], entries, more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let mut stream = Vec::new();
    for i in 0..n {
        record::write_header(&mut stream, &Header::File { id: i as u64 + 1, size: 5, mtime: Timestamp { seconds: 1_600_000_000, nanoseconds: 0 } }).unwrap();
        stream.extend_from_slice(b"hello");
        record::write_trailer(&mut stream, &Trailer { hash: *blake3::hash(b"hello").as_bytes(), unread: None }).unwrap();
    }
    if cut {
        record::write_header(&mut stream, &Header::File { id: n as u64 + 1, size: 1_000_000, mtime: Timestamp { seconds: 1_600_000_000, nanoseconds: 0 } }).unwrap();
        stream.extend_from_slice(&vec![7u8; 300_000]);
    }
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    if error_packet {
        protocol::send(&mut packets, &Control::Error { message: "source disk failed".into() }).unwrap();
    }
    let mut out = Vec::new();
    let result = destination::run(&mut &packets[..], &mut out, &opts(&root, false));
    println!("e_cut(n={n}, cut={cut}, error={error_packet}): run -> {:?}", result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    println!("   left in destination: {:?}", list(&root));
}

/// Directory 'a' whose finish fails, then 'b' whose finish is skipped.
fn e_finish() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("dst");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &Manifest::Root(entry(0, b"", Kind::Directory, 0, 0, 0o755))).unwrap();
    let entries = vec![
        entry(1, b"a", Kind::Directory, 0, 2_000_000_000, 0o755),
        entry(2, b"b", Kind::Directory, 0, 0, 0o750),
        entry(3, b"c", Kind::Directory, 0, 0, 0o711),
    ];
    protocol::send(&mut packets, &Manifest::Directory { path: vec![], entries, more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"a".to_vec(), entries: vec![], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"b".to_vec(), entries: vec![], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"c".to_vec(), entries: vec![], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let mut out = Vec::new();
    let result = destination::run(&mut &packets[..], &mut out, &opts(&root, false));
    println!("e_finish: run -> {:?}", result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    for d in ["a", "b", "c"] {
        let m = std::fs::metadata(root.join(d)).unwrap();
        println!("   {d}: mode {:o} mtime {}", m.permissions().mode() & 0o7777, m.mtime());
    }
    let m = std::fs::metadata(&root).unwrap();
    println!("   root: mode {:o} mtime {}", m.permissions().mode() & 0o7777, m.mtime());
}

/// fsys::set_mode_at on a symlink changes the link's target.
fn e_chmod_follows() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("root")).unwrap();
    std::fs::write(temp.path().join("outside"), "x").unwrap();
    std::fs::set_permissions(temp.path().join("outside"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(temp.path().join("outside"), temp.path().join("root/f")).unwrap();
    let dir = ferry::fsys::open_root(temp.path().join("root").as_os_str()).unwrap();
    let r = ferry::fsys::set_mode_at(&dir, b"f", 0o4777);
    let m = std::fs::metadata(temp.path().join("outside")).unwrap();
    println!("e_chmod_follows: set_mode_at -> {r:?}; outside file now mode {:o}", m.permissions().mode() & 0o7777);
}

/// Parts out of depth-first order: "", "a", "b", "a/x".
fn e_dfs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("dst");
    let mut packets = Vec::new();
    protocol::send(&mut packets, &Manifest::Root(entry(0, b"", Kind::Directory, 0, 0, 0o755))).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: vec![], entries: vec![
        entry(1, b"a", Kind::Directory, 0, 0, 0o755),
        entry(2, b"b", Kind::Directory, 0, 0, 0o755)], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"a".to_vec(), entries: vec![
        entry(3, b"x", Kind::Directory, 0, 0, 0o700)], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"b".to_vec(), entries: vec![], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::Directory { path: b"a/x".to_vec(), entries: vec![
        entry(4, b"f", Kind::Symlink { target: b"t".to_vec() }, 0, 0, 0o777)], more: false }).unwrap();
    protocol::send(&mut packets, &Manifest::End).unwrap();
    let mut stream = Vec::new();
    record::write_header(&mut stream, &Header::End).unwrap();
    protocol::send_data(&mut packets, &frame::encode(&stream, true, 3).unwrap()).unwrap();
    let mut out = Vec::new();
    let result = destination::run(&mut &packets[..], &mut out, &opts(&root, false));
    println!("e_dfs: run -> {:?}", result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    for d in ["a", "a/x", "b"] {
        let m = std::fs::metadata(root.join(d)).unwrap();
        println!("   {d}: mode {:o} mtime {}", m.permissions().mode() & 0o7777, m.mtime());
    }
    println!("   a/x/f exists: {}", std::fs::symlink_metadata(root.join("a/x/f")).is_ok());
}

fn main() {
    let only = std::env::args().nth(1);
    if only.as_deref() == Some("dfs") { e_dfs(); return; }
    e_cut(0, true, false);
    e_cut(5, true, false);
    e_cut(5, false, true);
    e_finish();
    e_chmod_follows();
    e_dfs();
}
