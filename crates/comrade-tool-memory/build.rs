//! Deflate the embedded embedding-model assets at build time.
//!
//! The int8 ONNX model (34 MB) and its tokenizer (0.7 MB) are compiled into the
//! binary by `include_bytes!`. Storing them raw costs their full size in the
//! release binary; deflating first and inflating in memory on first use saves
//! ~11 MB of binary (the int8 weights still compress ~30%). The raw files under
//! `assets/` remain the source of truth; the compressed copies go to `OUT_DIR`
//! and are what `src/semantic.rs` embeds.

use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/bge-small-en-v1.5-int8");
    let dst =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo")).join("assets");
    std::fs::create_dir_all(&dst).expect("create OUT_DIR/assets");

    // Re-run only when the assets change (not on every source edit).
    println!("cargo:rerun-if-changed={}", src.display());

    for entry in std::fs::read_dir(&src).expect("read assets dir") {
        let path = entry.expect("asset entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        println!("cargo:rerun-if-changed={}", path.display());
        if name.ends_with(".md") {
            continue;
        }
        let raw = std::fs::read(&path).expect("read asset");
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&raw).expect("deflate asset");
        let deflated = enc.finish().expect("finish deflate");
        std::fs::write(dst.join(format!("{name}.deflate")), &deflated).expect("write deflated asset");
    }
}
