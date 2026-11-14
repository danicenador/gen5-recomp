//! Records the commit gen5 was built from, so `gen5 version` and the build
//! stamp in each cache tell builds of the same version number apart.

use std::process::Command;

fn main() {
    let commit = std::env::var("GEN5_COMMIT").ok().or_else(|| {
        Command::new("git")
            .args(["rev-parse", "--short=10", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    });
    println!(
        "cargo:rustc-env=GEN5_COMMIT={}",
        commit.as_deref().unwrap_or("")
    );
    println!("cargo:rerun-if-env-changed=GEN5_COMMIT");
    // HEAD names the branch; the branch's ref (or packed-refs) moves on commit.
    // A missing path would rerun this script on every build, so only existing
    // ones are watched.
    let mut watched = vec![".git/HEAD".to_owned(), ".git/packed-refs".to_owned()];
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        watched.push(format!(".git/{reference}"));
    }
    for path in watched {
        if std::path::Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
