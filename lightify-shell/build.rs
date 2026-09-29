fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    // Embed the app manifest (per-monitor-v2 DPI awareness) via the MSVC linker —
    // no external resource compiler needed. Ensures the window renders at native
    // pixels on high-DPI / scaled displays instead of being bitmap-blurred by Windows.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    let manifest = manifest_dir.join("app.manifest");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());

    // Embed the application icon as a Win32 resource. This is the icon Explorer,
    // a pinned taskbar entry, Alt-Tab and any shortcut read off the .exe itself —
    // the runtime `Window { icon: ... }` in app.slint only dresses the live window,
    // so both are needed. Resource id 1 is the one Windows takes as the app icon.
    let ico = manifest_dir.join("assets").join("lightify.ico");
    println!("cargo:rerun-if-changed=assets/lightify.ico");
    if !ico.is_file() {
        println!("cargo:warning=assets/lightify.ico missing - building without an exe icon");
        return;
    }
    let Some(rc) = find_rc() else {
        // Not fatal: the binary still works, it just gets the default exe icon.
        // Say so rather than breaking someone's build over an icon.
        println!("cargo:warning=rc.exe not found - building without an exe icon");
        return;
    };

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("icon.rc");
    let res_path = out_dir.join("icon.res");
    // Icon + VERSIONINFO. The version block is what Task Manager's Details tab, the
    // file properties dialog and the installer all read for the product name - without
    // it the process shows up with no description at all.
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = ver.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (vmaj, vmin, vpat) =
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
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
         \x20     VALUE \"FileDescription\", \"Lightify\"\n\
         \x20     VALUE \"FileVersion\", \"{ver}\"\n\
         \x20     VALUE \"InternalName\", \"Lightify\"\n\
         \x20     VALUE \"OriginalFilename\", \"Lightify.exe\"\n\
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
    std::fs::write(&rc_path, rc_src).expect("write icon.rc");

    match std::process::Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()
    {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", res_path.display()),
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}) - building without an exe icon"),
        Err(e) => println!("cargo:warning=could not run rc.exe ({e}) - building without an exe icon"),
    }
}

/// Locate the Windows SDK resource compiler. It ships with the SDK rather than the
/// MSVC toolchain the linker comes from, so nothing in the build already knows where
/// it is; walk the SDK bin directories and take the newest.
#[cfg(windows)]
fn find_rc() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("LIGHTIFY_RC_EXE") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let arch = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for root in [std::env::var_os("ProgramFiles(x86)"), std::env::var_os("ProgramFiles")]
        .into_iter()
        .flatten()
    {
        let bin = std::path::PathBuf::from(root).join("Windows Kits").join("10").join("bin");
        if let Ok(entries) = std::fs::read_dir(&bin) {
            for e in entries.flatten() {
                let p = e.path().join(arch).join("rc.exe");
                if p.is_file() {
                    candidates.push(p);
                }
            }
        }
        // Older SDK layouts put rc.exe straight under bin/<arch>.
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
