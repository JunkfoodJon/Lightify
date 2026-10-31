//! lightify-pack: append the compressed payload to the setup stub.
//!
//!     lightify-pack --stub target\release\Lightify-Setup.exe --out Lightify-Setup-2.3.0.exe
//!                   --version 2.3.0 [--level 19] [--threads N] FILE [FILE ...]
//!
//! Each FILE is installed under its own file name. LICENSE.md is required (the wizard
//! shows it before Install), as is Lightify.exe. Called by
//! lightify-shell/scripts/build-installer.ps1.

use std::path::PathBuf;

use lightify_installer::payload;

fn usage() -> ! {
    eprintln!("usage: lightify-pack --stub <stub.exe> --out <setup.exe> --version <x.y.z> [--level N] [--threads N] FILE...");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut stub, mut out, mut version) = (None, None, None);
    let mut level = 19;
    let mut threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--stub" => stub = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--version" => version = args.next(),
            "--level" => level = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--threads" => threads = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            s if s.starts_with("--") => usage(),
            path => {
                let p = PathBuf::from(path);
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| usage());
                if !p.is_file() {
                    eprintln!("lightify-pack: not a file: {}", p.display());
                    std::process::exit(1);
                }
                if files.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    eprintln!("lightify-pack: {name} given twice");
                    std::process::exit(1);
                }
                files.push((name, p));
            }
        }
    }
    let (Some(stub), Some(out), Some(version)) = (stub, out, version) else { usage() };
    for need in [payload::LICENSE_NAME, "Lightify.exe"] {
        if !files.iter().any(|(n, _)| n.eq_ignore_ascii_case(need)) {
            eprintln!("lightify-pack: the payload must include {need}");
            std::process::exit(1);
        }
    }
    if out.exists() {
        let _ = std::fs::remove_file(&out);
    }
    let started = std::time::Instant::now();
    match payload::write_payload(&stub, &out, &version, &files, level, threads) {
        Ok(h) => {
            let packed = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            for f in &h.files {
                println!("  {:<28} {:>10.1} MB", f.name, f.size as f64 / 1048576.0);
            }
            println!(
                "packed {} file(s), {:.1} MB -> {} ({:.1} MB, zstd -{level}) in {:.0?}",
                h.files.len(),
                h.total_size() as f64 / 1048576.0,
                out.display(),
                packed as f64 / 1048576.0,
                started.elapsed()
            );
            // Read it back the way the stub will, so a broken payload never ships.
            match payload::PayloadReader::open(&out) {
                Ok(Some(mut r)) => {
                    while let Some(_) = r.next_file(&mut std::io::sink(), &mut |_| {}).unwrap_or_else(|e| {
                        eprintln!("lightify-pack: verify failed: {e}");
                        let _ = std::fs::remove_file(&out);
                        std::process::exit(1);
                    }) {}
                    println!("verified: payload reads back cleanly");
                }
                other => {
                    eprintln!("lightify-pack: verify failed: {:?}", other.err());
                    let _ = std::fs::remove_file(&out);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("lightify-pack: {e}");
            let _ = std::fs::remove_file(&out);
            std::process::exit(1);
        }
    }
}
