#[path = "src/build_info/source.rs"]
mod source;

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let files = source::source_files(&root).expect("list the source files");
    let hash = source::source_hash(&root, &files).expect("hash the source files");
    println!("cargo:rustc-env=FERRY_SOURCE_HASH={hash}");
    // The platform a release is built for, which `ferry update` downloads.
    let target = std::env::var("TARGET").expect("set by cargo");
    println!("cargo:rustc-env=FERRY_TARGET={target}");

    // A directory makes cargo rerun this script when anything under it changes.
    println!("cargo:rerun-if-changed={}", source::SOURCE_DIR);
    for file in source::ROOT_FILES {
        println!("cargo:rerun-if-changed={file}");
    }
}
