//! Compile-time cursor artwork.
//!
//! `cursor-icons/*.png` (when present) plus the drawn fallback become a Rust
//! table in `OUT_DIR` via `tools/gen_cursors.py`. The daemon never decodes an
//! image at runtime and never ships one: a cursor plane wants ARGB pixels, and
//! a table of them is what this produces.
//!
//! python3 is a build-time requirement, in the same spirit as any project that
//! generates sources. Without an icons directory the drawn shapes are used, so
//! a plain checkout still builds.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let script = manifest.join("tools/gen_cursors.py");
    let out = out_dir.join("cursor_art.rs");

    println!("cargo:rerun-if-changed=tools/gen_cursors.py");
    println!("cargo:rerun-if-changed=build.rs");

    let mut command = Command::new("python3");
    command.arg(&script).arg("--out").arg(&out);

    let icons = manifest.join("cursor-icons");
    if icons.is_dir() {
        // A directory: cargo tracks every file under it.
        println!("cargo:rerun-if-changed=cursor-icons");
        command.arg("--icons").arg(&icons);
    }

    let status = command.status().unwrap_or_else(|error| {
        panic!(
            "generating the cursor artwork needs python3 ({error}). \
             Install python3, or build with the icons directory removed."
        )
    });
    assert!(status.success(), "tools/gen_cursors.py failed");
    assert!(out.is_file(), "tools/gen_cursors.py produced no art");
}
