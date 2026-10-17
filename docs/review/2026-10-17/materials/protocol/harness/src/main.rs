use std::io::Write;
use std::time::Duration;

use ferry::destination::{self, Options};
use ferry::manifest::{Entry, Kind, Manifest, Timestamp};
use ferry::protocol::{self, PacketType};
use ferry::record::{self, Header, Trailer};
use ferry::{frame, mux};

fn entry(id: u64, name: &[u8], kind: Kind, size: u64) -> Entry {
    Entry {
        id,
        name: name.to_vec(),
        kind,
        size,
        mtime: Timestamp { seconds: 1_000_000, nanoseconds: 0 },
        mode: 0o755,
        uid: 0,
        gid: 0,
    }
}

fn manifest(out: &mut Vec<u8>, m: &Manifest) {
    out.extend(protocol::encode(m).unwrap());
}

fn data(out: &mut Vec<u8>, frame_bytes: &[u8]) {
    for piece in frame_bytes.chunks(mux::DATA_PACKET) {
        out.extend(protocol::encode_data(piece).unwrap());
    }
}

fn record_bytes(id: u64, content: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    record::write_header(&mut r, &Header::File { id, size: content.len() as u64, mtime: Timestamp { seconds: 1, nanoseconds: 0 } }).unwrap();
    r.extend_from_slice(content);
    record::write_trailer(&mut r, &Trailer { hash: *blake3::hash(content).as_bytes(), unread: None }).unwrap();
    r
}

fn options(root: &std::path::Path, dry_run: bool, delete: bool) -> Options {
    Options {
        root: root.as_os_str().as_encoded_bytes().to_vec(),
        dry_run,
        modify_window: 0,
        numeric_ids: true,
        fsync: false,
        delete,
        report: false,
    }
}

fn kinds(written: &[u8]) -> Vec<String> {
    let mut reader = written;
    let mut out = Vec::new();
    while let Ok(Some(p)) = protocol::receive(&mut reader) {
        let s = match p.kind {
            PacketType::Control => format!("Control::{:?}", p.decode::<protocol::Control>().unwrap()),
            PacketType::Results => format!("{:?}", p.decode::<protocol::Results>().unwrap()),
            PacketType::Needs => {
                let n = p.decode::<protocol::Needs>().unwrap();
                format!("Needs::{}", format!("{n:?}").chars().take(60).collect::<String>())
            }
            k => format!("{k:?}"),
        };
        out.push(s);
    }
    out
}

fn latency(base: &std::path::Path) {
    let root = base.join("e");
    std::fs::create_dir_all(&root).unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let (out_reader, out_writer) = std::io::pipe().unwrap();
    let started = std::time::Instant::now();
    let watcher = std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(out_reader);
        while let Ok(Some(p)) = protocol::receive(&mut r) {
            if p.kind == PacketType::Needs {
                println!("E  at {:>5} ms: {:?}", started.elapsed().as_millis(), p.decode::<protocol::Needs>().unwrap());
            }
        }
    });
    let feeder = std::thread::spawn(move || {
        let mut input = Vec::new();
        manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
        manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries: vec![entry(1, b"f", Kind::File, 3)], more: false });
        writer.write_all(&input).unwrap();
        println!("E  at {:>5} ms: sent the root's directory; the next message comes in 2 s", started.elapsed().as_millis());
        std::thread::sleep(Duration::from_millis(2000));
        let mut input = Vec::new();
        manifest(&mut input, &Manifest::End);
        writer.write_all(&input).unwrap();
    });
    let mut reader = reader;
    let mut out = out_writer;
    let _ = destination::run(&mut reader, &mut out, &options(&root, true, false));
    drop(out);
    feeder.join().unwrap();
    watcher.join().unwrap();
}

fn fifo(base: &std::path::Path) {
    use ferry::protocol::Needs;
    let root = base.join("fifo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f"), b"hello").unwrap();
    let (src_in, mut to_src) = std::io::pipe().unwrap();
    let (from_src, src_out) = std::io::pipe().unwrap();
    let root2 = root.clone();
    std::thread::spawn(move || {
        let mut from_src = std::io::BufReader::new(from_src);
        // Read the manifest to its end.
        loop {
            let p = protocol::receive(&mut from_src).unwrap().unwrap();
            if p.kind == PacketType::Manifest {
                let m = p.decode::<Manifest>().unwrap();
                if m == Manifest::End { break; }
            }
        }
        // The file becomes a FIFO before it is sent.
        std::fs::remove_file(root2.join("f")).unwrap();
        let status = std::process::Command::new("mkfifo").arg(root2.join("f")).status().unwrap();
        assert!(status.success());
        to_src.write_all(&protocol::encode(&Needs::Files(vec![1])).unwrap()).unwrap();
        to_src.write_all(&protocol::encode(&Needs::End).unwrap()).unwrap();
        let started = std::time::Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(p)) = protocol::receive(&mut from_src) {
                let _ = tx.send(p.kind);
            }
        });
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(kind) => println!("F  at {:>5} ms: Source sent {kind:?}", started.elapsed().as_millis()),
                Err(_) => {
                    println!("F  nothing from the Source for 5 s after it was asked for f, now a FIFO: hung");
                    std::process::exit(0);
                }
            }
        }
    });
    let options = ferry::source::Options {
        root: root.as_os_str(),
        dry_run: false,
        numeric_ids: true,
        compress: false,
        compress_level: 1,
        report: false,
        part_entries: 0,
        backlog_files: 0,
        paced_bytes: 0,
    };
    let mut src_in = src_in;
    let mut src_out = src_out;
    let r = ferry::source::run(&mut src_in, &mut src_out, &options);
    println!("F  Source returned {:?}", r.map(|_| ()));
}

fn truncated(base: &std::path::Path) {
    let root = base.join("g");
    std::fs::create_dir_all(&root).unwrap();
    let mut input = Vec::new();
    manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
    // 50 small files, then "big" (sorted after "f..").
    let mut entries: Vec<Entry> = (0..50).map(|i| entry(i + 1, format!("f{i:02}").as_bytes(), Kind::File, 3)).collect();
    entries.push(entry(51, b"zbig", Kind::File, 1 << 20));
    manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries, more: false });
    manifest(&mut input, &Manifest::End);
    // 50 whole records, then the big one's header and its first 100 KiB,
    // then the stream ends, as when the Source's connection is cut in the
    // middle of a file.
    let mut stream = Vec::new();
    for i in 0..50 {
        stream.extend(record_bytes(i + 1, b"abc"));
    }
    record::write_header(&mut stream, &Header::File { id: 51, size: 1 << 20, mtime: Timestamp { seconds: 1, nanoseconds: 0 } }).unwrap();
    stream.extend(vec![7u8; 100 * 1024]);
    data(&mut input, &frame::encode(&stream, false, 1).unwrap());
    let mut out = Vec::new();
    let r = destination::run(&mut input.as_slice(), &mut out, &options(&root, false, false));
    let names: Vec<String> = std::fs::read_dir(&root).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let temporaries: Vec<&String> = names.iter().filter(|n| n.contains(".ferry-")).collect();
    let finals = names.len() - temporaries.len();
    println!("G stream cut mid-record: returned {:?}; {} temporaries left (e.g. {:?}), {finals} files under their names", r.map(|_| "Ok").map_err(|e| format!("{e:#}")), temporaries.len(), temporaries.first());
}

fn main() {
    let base = std::path::PathBuf::from(std::env::args().nth(1).expect("base dir"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    if std::env::args().nth(2).as_deref() == Some("truncated") {
        truncated(&base);
        return;
    }
    if std::env::args().nth(2).as_deref() == Some("fifo") {
        fifo(&base);
        return;
    }
    if std::env::args().nth(2).as_deref() == Some("latency") {
        latency(&base);
        return;
    }

    // B: dry run, the stream ends in the middle of the manifest.
    {
        let root = base.join("b");
        std::fs::create_dir_all(&root).unwrap();
        let mut input = Vec::new();
        manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
        manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries: vec![entry(1, b"a", Kind::File, 3)], more: true });
        let mut out = Vec::new();
        let r = destination::run(&mut input.as_slice(), &mut out, &options(&root, true, false));
        println!("B dry run, EOF before Manifest::End: {:?}; sent {:?}", r.map(|_| "Ok"), kinds(&out));
    }

    // C: a needed file that never comes, and is never reported.
    {
        let root = base.join("c");
        std::fs::create_dir_all(&root).unwrap();
        let mut input = Vec::new();
        manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
        manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries: vec![entry(1, b"a", Kind::File, 3)], more: false });
        manifest(&mut input, &Manifest::End);
        let mut stream = Vec::new();
        record::write_header(&mut stream, &Header::End).unwrap();
        data(&mut input, &frame::encode(&stream, false, 1).unwrap());
        let mut out = Vec::new();
        let r = destination::run(&mut input.as_slice(), &mut out, &options(&root, false, false));
        println!("C needed file never sent: {:?}; a exists: {}; sent {:?}", r.map(|s| (s.failed, s.problems)), root.join("a").exists(), kinds(&out));
    }

    // A: a corrupt frame, then more frames: which error is returned?
    {
        let root = base.join("a");
        std::fs::create_dir_all(&root).unwrap();
        let mut input = Vec::new();
        manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
        manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries: vec![entry(1, b"a", Kind::File, 3)], more: false });
        manifest(&mut input, &Manifest::End);
        data(&mut input, &frame::encode(&record_bytes(1, b"abc"), false, 1).unwrap());
        // A zstd frame whose body is garbage.
        let mut bad = vec![1u8];
        bad.extend_from_slice(&100u32.to_le_bytes());
        bad.extend_from_slice(&4u32.to_le_bytes());
        bad.extend_from_slice(&[9, 9, 9, 9]);
        data(&mut input, &bad);
        for _ in 0..60 {
            data(&mut input, &frame::encode(&vec![0u8; 1000], false, 1).unwrap());
        }
        let mut out = Vec::new();
        let r = destination::run(&mut input.as_slice(), &mut out, &options(&root, false, false));
        println!("A corrupt frame: returned {:?}; sent {:?}", r.map(|_| "Ok").map_err(|e| format!("{e:#}")), kinds(&out));
    }

    // D: the writer fails for good; the main thread goes on applying the manifest (sync deletes).
    {
        let root = base.join("d");
        std::fs::create_dir_all(root.join("later")).unwrap();
        for i in 0..5 {
            std::fs::write(root.join("later").join(format!("extra{i}")), b"x").unwrap();
        }
        let (reader, mut writer) = std::io::pipe().unwrap();
        let feeder = std::thread::spawn(move || {
            let mut input = Vec::new();
            manifest(&mut input, &Manifest::Root(entry(0, b"", Kind::Directory, 0)));
            manifest(&mut input, &Manifest::Directory { path: Vec::new(), entries: vec![entry(1, b"f", Kind::File, 3), entry(2, b"later", Kind::Directory, 0)], more: false });
            writer.write_all(&input).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            // A record for an ID never asked for: the writer stops for good.
            let mut input = Vec::new();
            data(&mut input, &frame::encode(&record_bytes(99, b"zzz"), false, 1).unwrap());
            // Another manifest message, to let the main thread apply what it compared.
            manifest(&mut input, &Manifest::Names { users: vec![], groups: vec![] });
            writer.write_all(&input).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            let mut input = Vec::new();
            manifest(&mut input, &Manifest::Directory { path: b"later".to_vec(), entries: vec![], more: false });
            manifest(&mut input, &Manifest::Names { users: vec![], groups: vec![] });
            writer.write_all(&input).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            let mut input = Vec::new();
            manifest(&mut input, &Manifest::End);
            writer.write_all(&input).unwrap();
        });
        let mut out = Vec::new();
        let mut reader = reader;
        let r = destination::run(&mut reader, &mut out, &options(&root, false, true));
        feeder.join().unwrap();
        let left = std::fs::read_dir(root.join("later")).unwrap().count();
        println!("D writer failed: returned {:?}; extras left in later/: {left} of 5; sent {:?}", r.map(|_| "Ok").map_err(|e| format!("{e:#}")), kinds(&out));
    }
}
