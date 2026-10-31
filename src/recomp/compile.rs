//! Build the generated C into the game library the runtime loads.
//!
//! Files compile in parallel, and an object is reused when it is newer than
//! its source and the headers, so a rebuild after regenerating one module
//! only recompiles that module.

use crate::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

pub const LIBRARY_NAME: &str = "game.dll";

#[derive(Debug)]
pub struct BuildReport {
    pub library: PathBuf,
    pub compiled: usize,
    pub reused: usize,
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Compile every `.c` file in `recomp_dir` with `compiler` and link the
/// objects into `recomp_dir/build/game.dll`.
pub fn build_library(recomp_dir: &Path, compiler: &str) -> Result<BuildReport> {
    let build_dir = recomp_dir.join("build");
    fs::create_dir_all(&build_dir)
        .map_err(|error| format!("could not create '{}': {error}", build_dir.display()))?;
    let mut sources: Vec<PathBuf> = fs::read_dir(recomp_dir)
        .map_err(|error| format!("could not list '{}': {error}", recomp_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    if sources.is_empty() {
        return Err(format!(
            "no generated C in '{}' (run `codegen` first)",
            recomp_dir.display()
        ));
    }
    // Largest first, so the long compiles start early.
    sources.sort_by_key(|path| std::cmp::Reverse(fs::metadata(path).map_or(0, |m| m.len())));
    let headers_changed = ["gen5rt.h", "recomp.h"]
        .iter()
        .filter_map(|name| modified(&recomp_dir.join(name)))
        .max();

    let next = AtomicUsize::new(0);
    let reused = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    let objects: Vec<PathBuf> = sources
        .iter()
        .map(|source| {
            let name = source.file_name().unwrap().to_string_lossy();
            build_dir.join(format!("{name}.o"))
        })
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |count| count.get());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(source) = sources.get(index) else {
                        break;
                    };
                    let object = &objects[index];
                    let fresh = match (modified(object), modified(source)) {
                        (Some(object_time), Some(source_time)) => {
                            object_time >= source_time
                                && headers_changed.is_none_or(|header| object_time >= header)
                        }
                        _ => false,
                    };
                    if fresh {
                        reused.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let output = Command::new(compiler)
                        .args(["-O1", "-fwrapv", "-fno-strict-aliasing", "-c"])
                        .arg(source)
                        .arg("-o")
                        .arg(object)
                        .output();
                    let failure = match output {
                        Ok(output) if output.status.success() => None,
                        Ok(output) => Some(format!(
                            "{} failed:\n{}",
                            source.display(),
                            String::from_utf8_lossy(&output.stderr)
                        )),
                        Err(error) => Some(format!("could not run {compiler}: {error}")),
                    };
                    if let Some(failure) = failure {
                        errors.lock().unwrap().push(failure);
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    // A response file keeps the link command under the Windows length limit.
    let response = build_dir.join("objects.rsp");
    let list: String = objects
        .iter()
        .map(|object| format!("\"{}\"\n", object.display().to_string().replace('\\', "/")))
        .collect();
    fs::write(&response, list)
        .map_err(|error| format!("could not write response file: {error}"))?;
    let library = build_dir.join(LIBRARY_NAME);
    let output = Command::new(compiler)
        .arg("-shared")
        .arg("-o")
        .arg(&library)
        .arg(format!("@{}", response.display()))
        .output()
        .map_err(|error| format!("could not run {compiler}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "linking {} failed:\n{}",
            library.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let reused = reused.into_inner();
    Ok(BuildReport {
        library,
        compiled: sources.len() - reused,
        reused,
    })
}
