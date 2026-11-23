use sha2::{Digest, Sha256};
use std::{path::{Path, PathBuf}, process::Command};

fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source directory") {
        let entry = entry.expect("source entry");
        let kind = entry.file_type().expect("source type");
        if kind.is_dir() { sources(&entry.path(), files); }
        else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "rs" || ext == "json") {
            files.push(entry.path());
        }
    }
}
fn main() {
    // Enumerate source roots explicitly. Account configuration, environment,
    // captures and target artifacts never enter the digest.
    let mut files: Vec<PathBuf> = ["Cargo.toml", "Cargo.lock", "build.rs", "vendor/ost/Cargo.toml"]
        .into_iter().map(PathBuf::from).collect();
    for dir in ["src", "vendor/ost/src", "locales"] {
        println!("cargo:rerun-if-changed={dir}");
        sources(Path::new(dir), &mut files);
    }
    files.sort();
    let mut hash = Sha256::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.to_str().expect("UTF-8 source path");
        let bytes = std::fs::read(&path).expect("source read");
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    println!("cargo:rustc-env=TEAMS_SOURCE_DIGEST={:x}", hash.finalize());
    let revision = Command::new("git").args(["rev-parse", "HEAD"]).output().ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| matches!(text.len(),40|64) && text.bytes().all(|byte|byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rustc-env=TEAMS_BUILD_REVISION={revision}");
}
