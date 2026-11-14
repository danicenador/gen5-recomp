//! Build the generated C into the game library the runtime loads.
//!
//! Files compile in parallel, and an object is reused when it is newer than
//! its source and the headers, so a rebuild after regenerating one module
//! only recompiles that module.

use crate::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

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

/// Reports `(files done, files in all)` as the build goes.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);

/// What `build_library_with` needs beyond the directory.
pub struct BuildOptions<'a> {
    pub compiler: &'a str,
    /// Set to stop the build: running compilers are killed and the library is
    /// not linked.
    pub cancel: Option<&'a AtomicBool>,
    pub progress: Option<Progress<'a>>,
}

/// The message a cancelled build fails with.
pub const CANCELLED: &str = "cancelled";

/// Compile every `.c` file in `recomp_dir` with `compiler` and link the
/// objects into `recomp_dir/build/game.dll`.
pub fn build_library(recomp_dir: &Path, compiler: &str) -> Result<BuildReport> {
    build_library_with(
        recomp_dir,
        &BuildOptions {
            compiler,
            cancel: None,
            progress: None,
        },
    )
}

/// `build_library` with cancellation and progress reports.
pub fn build_library_with(recomp_dir: &Path, options: &BuildOptions) -> Result<BuildReport> {
    let compiler = options.compiler;
    let cancelled = || {
        options
            .cancel
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    };
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
    let done = AtomicUsize::new(0);
    let finished = || {
        let count = done.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(progress) = options.progress {
            progress(count, sources.len());
        }
    };
    if let Some(progress) = options.progress {
        progress(0, sources.len());
    }
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
                    if cancelled() {
                        break;
                    }
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
                        finished();
                        continue;
                    }
                    match compile(compiler, source, object, &cancelled) {
                        Ok(true) => finished(),
                        Ok(false) => break,
                        Err(failure) => errors.lock().unwrap().push(failure),
                    }
                }
            });
        }
    });
    if cancelled() {
        return Err(CANCELLED.to_owned());
    }
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

/// Compiles one file, with the compiler's messages kept beside the object
/// (`<object>.log`). Returns false when the build was cancelled meanwhile.
fn compile(
    compiler: &str,
    source: &Path,
    object: &Path,
    cancelled: &dyn Fn() -> bool,
) -> std::result::Result<bool, String> {
    let log_path = object.with_extension("log");
    let log = fs::File::create(&log_path)
        .map_err(|error| format!("could not create '{}': {error}", log_path.display()))?;
    let mut child = Command::new(compiler)
        .args(["-O1", "-fwrapv", "-fno-strict-aliasing", "-c"])
        .arg(source)
        .arg("-o")
        .arg(object)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|error| format!("could not run {compiler}: {error}"))?;
    let status = loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            // A half-written object must not look fresh to the next build.
            let _ = fs::remove_file(object);
            return Ok(false);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("could not wait for {compiler}: {error}")),
        }
    };
    if status.success() {
        return Ok(true);
    }
    let _ = fs::remove_file(object);
    Err(format!(
        "{} failed:
{}",
        source.display(),
        fs::read_to_string(&log_path).unwrap_or_default()
    ))
}
