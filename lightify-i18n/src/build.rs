//! For build scripts: find a crate's catalogs and generate the list the Rust side embeds.
//!
//! Catalogs live where Slint's bundler looks for them,
//! `<crate>/translations/<lang>/LC_MESSAGES/<domain>.po`, so one file per language serves
//! both. `emit` writes `$OUT_DIR/languages.rs` (`pub static LANGS: &[lightify_i18n::Lang]`)
//! and returns the folder to bundle; with no catalogs, the caller compiles its `.slint`
//! without bundling (Slint refuses to bundle an empty folder) and `LANGS` is empty.
//!
//! `LIGHTIFY_PSEUDO=1` in a build's environment adds the pseudo language [`PSEUDO`]
//! (LANGUAGE-PLAN.md, Phase 1): every string longer, accented and bracketed, to find
//! clipped or untranslated text. It is generated from the source into `OUT_DIR`, never
//! into `translations/`, so only a build that asks for it has it. Build it into its own
//! target dir (`CARGO_TARGET_DIR=target/pseudo`) so `target/release` never holds it;
//! build-installer.ps1 refuses an exe that has it.

use std::path::{Path, PathBuf};

/// The catalogs under `dir` for `domain`, as (language code, file), sorted by code.
pub fn find(dir: &Path, domain: &str) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let po = e.path().join("LC_MESSAGES").join(format!("{domain}.po"));
            po.is_file().then(|| (e.file_name().to_string_lossy().into_owned(), po))
        })
        .collect();
    found.sort();
    found
}

/// The pseudo language's code (Windows' own pseudo-locale name); select it with `--lang`.
pub const PSEUDO: &str = "qps-ploc";

/// Finds the catalogs in `<manifest dir>/translations`, writes `$OUT_DIR/languages.rs`,
/// tells Cargo to rebuild when they change, and returns the folder Slint should bundle
/// (None while there is nothing to bundle).
pub fn emit(domain: &str) -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut dir = manifest.join("translations");
    println!("cargo:rerun-if-changed={}", dir.display());
    println!("cargo:rerun-if-env-changed=LIGHTIFY_PSEUDO");
    let mut found = find(&dir, domain);
    for (_, po) in &found {
        println!("cargo:rerun-if-changed={}", po.display());
    }
    if std::env::var("LIGHTIFY_PSEUDO").is_ok_and(|v| !v.is_empty() && v != "0") {
        // The real catalogs plus the pseudo one, in a folder of this build's own.
        let bundle = out_dir.join("translations");
        let _ = std::fs::remove_dir_all(&bundle);
        let file = |code: &str| bundle.join(code).join("LC_MESSAGES").join(format!("{domain}.po"));
        for (code, po) in &found {
            std::fs::create_dir_all(file(code).parent().unwrap()).expect("create translations");
            std::fs::copy(po, file(code)).expect("copy catalog");
        }
        // One extractor for the template and the pseudo catalog: scripts/extract-strings.py.
        let script = manifest.join("..").join("scripts").join("extract-strings.py");
        let status = std::process::Command::new("python")
            .arg(&script)
            .args(["--pseudo", domain])
            .arg(file(PSEUDO))
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            panic!("LIGHTIFY_PSEUDO: `python {} --pseudo` failed ({status:?})", script.display());
        }
        println!("cargo:rerun-if-changed={}", script.display());
        println!("cargo:rerun-if-changed={}", manifest.join("ui").display());
        println!("cargo:rerun-if-changed={}", manifest.join("src").display());
        println!("cargo:warning=pseudo language {PSEUDO} bundled (LIGHTIFY_PSEUDO): a test build, never ship it");
        dir = bundle;
        found = find(&dir, domain);
    }
    let mut src = String::from("pub static LANGS: &[lightify_i18n::Lang] = &[\n");
    for (code, po) in &found {
        src.push_str(&format!(
            "    lightify_i18n::Lang {{ code: {code:?}, po: include_str!({:?}) }},\n",
            po.display().to_string()
        ));
    }
    src.push_str("];\n");
    std::fs::write(out_dir.join("languages.rs"), src).expect("write languages.rs");
    (!found.is_empty()).then_some(dir)
}
