fn main() {
    // Translations, as lightify-shell/build.rs does them (LANGUAGE-PLAN.md).
    let bundle = lightify_i18n::build::emit("lightify-installer");
    let mut config = slint_build::CompilerConfiguration::new()
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    if let Some(dir) = bundle {
        config = config.with_bundled_translations(dir);
    }
    slint_build::compile_with_config("ui/installer.slint", config).unwrap();

    // The version shown before a payload is read (and in the exe's VERSIONINFO) is the
    // app's own, read from lightify-shell/Cargo.toml so the two can never drift. The
    // payload header carries the authoritative version at run time.
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let shell_toml = manifest_dir.join("..").join("lightify-shell").join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", shell_toml.display());
    let ver = std::fs::read_to_string(&shell_toml)
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.trim_start().starts_with("version") && l.contains('"'))
                .and_then(|l| l.split('"').nth(1).map(str::to_string))
        })
        .unwrap_or_else(|| "0.0.0".into());
    println!("cargo:rustc-env=LIGHTIFY_APP_VERSION={ver}");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    // Manifest (asInvoker + per-monitor-v2 DPI), embedded by the MSVC linker, for the
    // setup stub only - the release tools are console programs.
    let manifest = manifest_dir.join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bin=Lightify-Setup=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=Lightify-Setup=/MANIFESTINPUT:{}", manifest.display());

    // App icon + VERSIONINFO, the same way lightify-shell/build.rs does it.
    let ico = manifest_dir.join("..").join("lightify-shell").join("assets").join("lightify.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if !ico.is_file() {
        println!("cargo:warning=lightify-shell/assets/lightify.ico missing - building without an exe icon");
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found - building without an exe icon");
        return;
    };
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("setup.rc");
    let res_path = out_dir.join("setup.res");
    let mut parts = ver.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (vmaj, vmin, vpat) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let rc_src = format!(
        "1 ICON \"{ico}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {vmaj},{vmin},{vpat},0\n\
         PRODUCTVERSION {vmaj},{vmin},{vpat},0\n\
         FILEOS 0x4L\n\
         FILETYPE 0x1L\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904B0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"CompanyName\", \"Lightify\"\n\
         \x20     VALUE \"FileDescription\", \"Lightify Setup\"\n\
         \x20     VALUE \"FileVersion\", \"{ver}\"\n\
         \x20     VALUE \"InternalName\", \"Lightify-Setup\"\n\
         \x20     VALUE \"OriginalFilename\", \"Lightify-Setup.exe\"\n\
         \x20     VALUE \"ProductName\", \"Lightify\"\n\
         \x20     VALUE \"ProductVersion\", \"{ver}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
        ico = ico.display().to_string().replace('\\', "\\\\"),
    );
    std::fs::write(&rc_path, rc_src).expect("write setup.rc");
    match std::process::Command::new(&rc).arg("/nologo").arg("/fo").arg(&res_path).arg(&rc_path).status() {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bin=Lightify-Setup={}", res_path.display()),
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}) - building without an exe icon"),
        Err(e) => println!("cargo:warning=could not run rc.exe ({e}) - building without an exe icon"),
    }
}

/// The Windows SDK resource compiler (same lookup as lightify-shell/build.rs).
#[cfg(windows)]
fn find_rc() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("LIGHTIFY_RC_EXE") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let arch = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") { "arm64" } else { "x64" };
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for root in [std::env::var_os("ProgramFiles(x86)"), std::env::var_os("ProgramFiles")].into_iter().flatten() {
        let bin = std::path::PathBuf::from(root).join("Windows Kits").join("10").join("bin");
        if let Ok(entries) = std::fs::read_dir(&bin) {
            for e in entries.flatten() {
                let p = e.path().join(arch).join("rc.exe");
                if p.is_file() {
                    candidates.push(p);
                }
            }
        }
        let flat = bin.join(arch).join("rc.exe");
        if flat.is_file() {
            candidates.push(flat);
        }
    }
    candidates.sort();
    candidates.pop()
}

#[cfg(not(windows))]
fn find_rc() -> Option<std::path::PathBuf> {
    None
}
