//! Self-update: a signed release manifest, a verified download, and the hand-off to
//! the installer.
//!
//! The flow, end to end:
//!
//! 1. [`check`] fetches `latest.json` from lightify.stream and verifies its ed25519
//!    signature against [`PUBLIC_KEY`], which is compiled in. An unsigned, re-signed or
//!    edited manifest is rejected outright, so the server (or anything between it and
//!    the user) can at worst withhold an update, never substitute one.
//! 2. [`download`] fetches the installer the manifest names into
//!    `%LOCALAPPDATA%\Lightify\updates\` and checks its size and SHA-256 against the
//!    signed values. Next to it goes the signed manifest itself, so a later launch can
//!    re-verify the file without the network ([`pending`]).
//! 3. [`launch_update`] starts that installer in `--update` mode and the app exits; the
//!    installer waits for this process to go, replaces the files and relaunches.
//!
//! Signing happens at release time with `lightify-sign` (lightify-installer); the
//! private key never lives in the repo.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

pub const MANIFEST_URL: &str = "https://lightify.stream/downloads/latest.json";

/// The release-signing public key (ed25519). The matching private key lives outside
/// the repo, at `%USERPROFILE%\.lightify-release\update-signing.key`.
/// Hex: 435a7e3ba5322d104608ddc6a74d86cb087c8994a4c60f3e1b03b1b97487c786
pub const PUBLIC_KEY: [u8; 32] = [
    0x43, 0x5a, 0x7e, 0x3b, 0xa5, 0x32, 0x2d, 0x10, 0x46, 0x08, 0xdd, 0xc6, 0xa7, 0x4d, 0x86, 0xcb,
    0x08, 0x7c, 0x89, 0x94, 0xa4, 0xc6, 0x0f, 0x3e, 0x1b, 0x03, 0xb1, 0xb9, 0x74, 0x87, 0xc7, 0x86,
];

/// A release the signed manifest offers.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    url: String,
    sha256: String,
    size: u64,
    sig: String,
}

/// The exact bytes the signature covers. `lightify-sign` builds the same string; a
/// shared test vector in both crates keeps the two from drifting.
pub fn signed_message(version: &str, url: &str, sha256: &str, size: u64) -> String {
    format!("lightify-update\n{version}\n{url}\n{sha256}\n{size}")
}

/// Manifest texts verified in this process, so [`download`] can store the signed
/// manifest next to the installer (that is what lets [`pending`] re-verify the file
/// on a later launch) without widening [`Release`].
static VERIFIED: Mutex<Vec<String>> = Mutex::new(Vec::new());

// ── Versions ────────────────────────────────────────────────────────────────

/// `x.y.z` as numbers. A leading `v` and any `-pre`/`+build` suffix are ignored;
/// missing parts count as 0. `None` for anything non-numeric.
fn parse_version(s: &str) -> Option<[u64; 3]> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    if core.is_empty() {
        return None;
    }
    let mut out = [0u64; 3];
    for (i, part) in core.split('.').enumerate() {
        if i >= 3 {
            return None;
        }
        out[i] = part.parse().ok()?;
    }
    Some(out)
}

/// Is `candidate` a strictly newer version than `current`? Unparseable = no.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        // The same x.y.z: a release is newer than its pre-release (`0.2.0` after
        // `0.2.0-rc1`), never the other way round (BUGS-PLAN AR2-15). Build metadata
        // (`+…`) doesn't count.
        (Some(a), Some(b)) if a == b => is_prerelease(current) && !is_prerelease(candidate),
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn is_prerelease(s: &str) -> bool {
    s.trim().split('+').next().is_some_and(|v| v.contains('-'))
}

// ── Manifest ────────────────────────────────────────────────────────────────

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn decode_sig(s: &str) -> Option<[u8; 64]> {
    let s = s.trim();
    let bytes = if s.len() == 128 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        decode_hex(s)?
    } else {
        base64::engine::general_purpose::STANDARD.decode(s).ok()?
    };
    bytes.try_into().ok()
}

fn is_local(url: &str) -> bool {
    !(url.starts_with("https://") || url.starts_with("http://"))
}

/// Parse a manifest and verify its signature with `key`. `allow_insecure` permits an
/// `http://` or local-path installer URL (only for a `LIGHTIFY_UPDATE_URL` test
/// override); a real manifest must point at https.
fn parse_and_verify(text: &str, key: &[u8; 32], allow_insecure: bool) -> Result<Release, String> {
    let m: Manifest = serde_json::from_str(text).map_err(|e| format!("update manifest: {e}"))?;
    let sha256 = m.sha256.trim().to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("update manifest: sha256 is not 64 hex digits".into());
    }
    if parse_version(&m.version).is_none()
        || !m.version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
    {
        return Err(format!("update manifest: bad version {:?}", m.version));
    }
    let vk = VerifyingKey::from_bytes(key).map_err(|_| "update: bad public key".to_string())?;
    let sig = decode_sig(&m.sig).ok_or("update manifest: signature is not 64 bytes of hex/base64")?;
    let msg = signed_message(&m.version, &m.url, &m.sha256, m.size);
    vk.verify_strict(msg.as_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| "update manifest: signature check failed".to_string())?;
    if !allow_insecure && !m.url.starts_with("https://") {
        return Err("update manifest: installer url is not https".into());
    }
    if m.size == 0 {
        return Err("update manifest: size is 0".into());
    }
    Ok(Release { version: m.version, url: m.url, sha256, size: m.size })
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Lightify-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        // Per read, not for the whole transfer (a slow but moving download is fine):
        // a connection that goes silent mid-download used to leave the state on
        // "downloading" for the rest of the session, refusing every later check.
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("update: http client: {e}"))
}

fn local_path(src: &str) -> PathBuf {
    let s = src.strip_prefix("file:///").or_else(|| src.strip_prefix("file://")).unwrap_or(src);
    PathBuf::from(s)
}

async fn fetch_text(src: &str) -> Result<String, String> {
    if is_local(src) {
        return std::fs::read_to_string(local_path(src)).map_err(|e| format!("update manifest {src}: {e}"));
    }
    let resp = http_client()?
        .get(src)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("update manifest: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update manifest: HTTP {}", resp.status().as_u16()));
    }
    resp.text().await.map_err(|e| format!("update manifest: {e}"))
}

async fn check_with_key(current_version: &str, key: &[u8; 32]) -> Result<Option<Release>, String> {
    let override_src = std::env::var("LIGHTIFY_UPDATE_URL").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let src = override_src.as_deref().unwrap_or(MANIFEST_URL);
    let text = fetch_text(src).await?;
    let rel = parse_and_verify(&text, key, override_src.is_some())?;
    if let Ok(mut v) = VERIFIED.lock() {
        if !v.contains(&text) {
            v.push(text);
        }
    }
    Ok(is_newer(&rel.version, current_version).then_some(rel))
}

/// None = up to date. Honors env LIGHTIFY_UPDATE_URL (http(s) URL or a local file path) for testing.
/// Verifies the manifest's ed25519 signature with the public key compiled in; rejects on failure.
pub async fn check(current_version: &str) -> Result<Option<Release>, String> {
    check_with_key(current_version, &PUBLIC_KEY).await
}

// ── Download ────────────────────────────────────────────────────────────────

/// `%LOCALAPPDATA%\Lightify\updates`. `LIGHTIFY_UPDATES_DIR` overrides it (tests).
pub fn updates_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LIGHTIFY_UPDATES_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Lightify")
        .join("updates")
}

fn installer_name(version: &str) -> String {
    format!("Lightify-Setup-{version}.exe")
}

fn sidecar_name(version: &str) -> String {
    format!("Lightify-Setup-{version}.json")
}

fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        n_total += n as u64;
    }
    Ok((format!("{:x}", h.finalize()), n_total))
}

fn verified_file(path: &Path, rel: &Release) -> bool {
    matches!(sha256_file(path), Ok((hash, len)) if len == rel.size && hash == rel.sha256)
}

/// The signed manifest text for `rel`, if this process verified one.
fn manifest_text_for(rel: &Release) -> Option<String> {
    let v = VERIFIED.lock().ok()?;
    v.iter()
        .rev()
        .find(|t| {
            serde_json::from_str::<Manifest>(t).is_ok_and(|m| {
                m.version == rel.version && m.url == rel.url && m.sha256.trim().eq_ignore_ascii_case(&rel.sha256) && m.size == rel.size
            })
        })
        .cloned()
}

/// Where downloaded bytes go: the `.part` file, the running hash, the progress
/// callback. Owns everything it touches, so the download future stays `Send`.
struct Sink<P> {
    out: std::fs::File,
    hasher: Sha256,
    done: u64,
    total: u64,
    progress: P,
}

impl<P: Fn(u64, u64)> Sink<P> {
    fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        use std::io::Write;
        self.done += chunk.len() as u64;
        if self.done > self.total {
            return Err("update: download is larger than the signed size".into());
        }
        self.hasher.update(chunk);
        self.out.write_all(chunk).map_err(|e| format!("update: write: {e}"))?;
        (self.progress)(self.done, self.total);
        Ok(())
    }
}

async fn fetch_into<P: Fn(u64, u64)>(rel: &Release, sink: &mut Sink<P>) -> Result<(), String> {
    if is_local(&rel.url) {
        use std::io::Read;
        let mut f = std::fs::File::open(local_path(&rel.url)).map_err(|e| format!("update: {}: {e}", rel.url))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).map_err(|e| format!("update: read: {e}"))?;
            if n == 0 {
                return Ok(());
            }
            sink.push(&buf[..n])?;
        }
    }
    let mut resp = http_client()?.get(&rel.url).send().await.map_err(|e| format!("update: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update: HTTP {}", resp.status().as_u16()));
    }
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("update: {e}"))? {
        sink.push(&chunk)?;
    }
    Ok(())
}

async fn download_into(
    dir: &Path,
    rel: &Release,
    progress: impl Fn(u64, u64) + Send + 'static,
) -> Result<PathBuf, String> {
    use std::io::Write;
    if parse_version(&rel.version).is_none()
        || !rel.version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
    {
        return Err(format!("update: bad version {:?}", rel.version));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("update: {}: {e}", dir.display()))?;
    let dest = dir.join(installer_name(&rel.version));
    let sidecar = dir.join(sidecar_name(&rel.version));
    let write_sidecar = || {
        if let Some(text) = manifest_text_for(rel) {
            let _ = std::fs::write(&sidecar, text);
        }
    };

    if dest.is_file() && verified_file(&dest, rel) {
        write_sidecar();
        progress(rel.size, rel.size);
        return Ok(dest);
    }

    let part = dir.join(format!("{}.part", installer_name(&rel.version)));
    let out = std::fs::File::create(&part).map_err(|e| format!("update: {}: {e}", part.display()))?;
    let mut sink = Sink { out, hasher: Sha256::new(), done: 0, total: rel.size, progress };
    let result = fetch_into(rel, &mut sink).await.and_then(|_| {
        let Sink { mut out, hasher, done, total, .. } = sink;
        out.flush().map_err(|e| format!("update: write: {e}"))?;
        drop(out);
        if done != total {
            return Err(format!("update: got {done} of {total} bytes"));
        }
        if format!("{:x}", hasher.finalize()) != rel.sha256 {
            return Err("update: downloaded file failed the SHA-256 check".into());
        }
        let _ = std::fs::remove_file(&dest);
        std::fs::rename(&part, &dest).map_err(|e| format!("update: {}: {e}", dest.display()))
    });
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    write_sidecar();
    Ok(dest)
}

/// Downloads to %LOCALAPPDATA%\Lightify\updates\Lightify-Setup-<ver>.exe (url may also be a local path when testing),
/// verifies sha256; reuses an already-downloaded verified file. progress(done, total).
pub async fn download(rel: &Release, progress: impl Fn(u64, u64) + Send + 'static) -> Result<PathBuf, String> {
    download_into(&updates_dir(), rel, progress).await
}

// ── Pending ─────────────────────────────────────────────────────────────────

fn pending_in(dir: &Path, current_version: &str, key: &[u8; 32]) -> Option<(String, PathBuf)> {
    let mut best: Option<([u64; 3], String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(ver) = name.strip_prefix("Lightify-Setup-").and_then(|r| r.strip_suffix(".exe")) else {
            continue;
        };
        let path = entry.path();
        let sidecar = dir.join(sidecar_name(ver));
        if !is_newer(ver, current_version) {
            // Already installed (or older): the file has done its job.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&sidecar);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&sidecar) else { continue };
        let Ok(rel) = parse_and_verify(&text, key, true) else { continue };
        if rel.version != ver || !verified_file(&path, &rel) {
            continue;
        }
        let pv = parse_version(ver)?;
        if best.as_ref().is_none_or(|(b, _, _)| pv > *b) {
            best = Some((pv, rel.version, path));
        }
    }
    best.map(|(_, v, p)| (v, p))
}

/// A verified installer for a version newer than current already sitting in the updates folder, if any.
///
/// Re-hashes the file (tens of MB), so call it off the UI thread. Installers for the
/// current or older versions are deleted as a side effect.
pub fn pending(current_version: &str) -> Option<(String, PathBuf)> {
    pending_in(&updates_dir(), current_version, &PUBLIC_KEY)
}

// ── Hand-off ────────────────────────────────────────────────────────────────

/// Spawns `<installer> --update --wait-pid <this pid> --relaunch --dir <this app's folder>`
/// detached. Caller then exits the app. `--dir` says which install to update when a PC has
/// both a per-user and an all-users one.
pub fn launch_update(installer: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new(installer);
    cmd.arg("--update")
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--relaunch");
    if let Some(app_dir) = std::env::current_exe().ok().as_deref().and_then(Path::parent) {
        cmd.arg("--dir").arg(app_dir);
    }
    // Not the install dir: nothing should hold a handle inside it while it is replaced.
    if let Some(dir) = installer.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| format!("update: could not start {}: {e}", installer.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const TEST_SEED: [u8; 32] = [7u8; 32];

    fn test_key() -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(&TEST_SEED);
        let pk = sk.verifying_key().to_bytes();
        (sk, pk)
    }

    fn manifest(sk: &SigningKey, version: &str, url: &str, sha256: &str, size: u64) -> String {
        let sig = sk.sign(signed_message(version, url, sha256, size).as_bytes());
        serde_json::json!({
            "version": version, "url": url, "sha256": sha256, "size": size,
            "sig": base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        })
        .to_string()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lightify-update-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const URL: &str = "https://lightify.stream/downloads/Lightify_2.3.0_x64-setup.exe";

    /// The shell spawns these on a multi-threaded runtime.
    #[allow(dead_code)]
    fn futures_are_send() {
        fn send<T: Send>(_: T) {}
        let rel = Release { version: String::new(), url: String::new(), sha256: String::new(), size: 0 };
        send(check("1.0.0"));
        send(download(&rel, |_, _| {}));
    }

    /// Release check, run by hand (and by scripts/stage-release.ps1):
    /// `LIGHTIFY_UPDATE_URL=<latest.json> cargo test release_manifest -- --ignored`
    /// proves a manifest written by `lightify-sign` verifies against the key compiled
    /// into the app.
    #[tokio::test]
    #[ignore]
    async fn release_manifest_verifies_with_compiled_key() {
        let rel = check("0.0.0").await.expect("manifest verifies with the release key");
        let rel = rel.expect("a release");
        println!("verified {} {} {} bytes", rel.version, rel.url, rel.size);
    }

    #[test]
    fn compiled_public_key_matches_its_hex() {
        assert_eq!(hex(&PUBLIC_KEY), "435a7e3ba5322d104608ddc6a74d86cb087c8994a4c60f3e1b03b1b97487c786");
        assert!(VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }

    #[test]
    fn version_compare_is_numeric() {
        assert!(is_newer("2.3.0", "2.2.4"));
        assert!(is_newer("2.10.0", "2.9.9"));
        assert!(is_newer("10.0.0", "9.99.99"));
        assert!(is_newer("v2.2.5", "2.2.4"));
        assert!(is_newer("2.3", "2.2.9"));
        assert!(!is_newer("2.2.4", "2.2.4"));
        assert!(!is_newer("2.2.3", "2.2.4"));
        assert!(!is_newer("2.2.4-beta", "2.2.4"));
        assert!(!is_newer("garbage", "2.2.4"));
        assert!(!is_newer("2.3.0", "garbage"));
        assert!(!is_newer("1.2.3.4", "1.0.0"));
        // AR2-15: a release follows its pre-release, not the reverse; build tags don't count.
        assert!(is_newer("0.2.0", "0.2.0-rc1"));
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
        assert!(!is_newer("0.2.0+build5", "0.2.0"));
        assert!(is_newer("0.2.1-rc1", "0.2.0"));
    }

    /// Same vector as lightify-installer's lightify-sign test: both sides must build
    /// the identical message and therefore the identical (deterministic) signature.
    #[test]
    fn signature_test_vector() {
        let (sk, _) = test_key();
        let msg = signed_message("2.3.0", URL, SHA, 123);
        assert_eq!(msg, format!("lightify-update\n2.3.0\n{URL}\n{SHA}\n123"));
        let sig = hex(&sk.sign(msg.as_bytes()).to_bytes());
        assert_eq!(sig, SIG_VECTOR);
    }
    /// ed25519(seed = [7; 32]) over the message above; cross-checked with an
    /// independent implementation (Python `cryptography`).
    const SIG_VECTOR: &str = "138266a7b3a876bd71b65d078693087369ed858a3ca8821818a8f9f3915470058c985fb978e8f238bd26747473265eaedcdb9de9913203ad442315cd56b9d203";

    #[test]
    fn signature_accepted_and_wrong_key_rejected() {
        let (sk, pk) = test_key();
        let text = manifest(&sk, "2.3.0", URL, SHA, 123);
        let rel = parse_and_verify(&text, &pk, false).expect("valid manifest");
        assert_eq!(rel.version, "2.3.0");
        assert_eq!(rel.size, 123);
        assert_eq!(rel.sha256, SHA);
        // The real release key did not sign this.
        assert!(parse_and_verify(&text, &PUBLIC_KEY, false).is_err());
        // Hex signatures are accepted too.
        let sig = sk.sign(signed_message("2.3.0", URL, SHA, 123).as_bytes());
        let hex_text = serde_json::json!({"version":"2.3.0","url":URL,"sha256":SHA,"size":123,"sig":hex(&sig.to_bytes())}).to_string();
        assert!(parse_and_verify(&hex_text, &pk, false).is_ok());
        // Garbage signature.
        let bad = serde_json::json!({"version":"2.3.0","url":URL,"sha256":SHA,"size":123,"sig":"AAAA"}).to_string();
        assert!(parse_and_verify(&bad, &pk, false).is_err());
    }

    #[test]
    fn tampered_fields_rejected() {
        let (sk, pk) = test_key();
        let good: serde_json::Value = serde_json::from_str(&manifest(&sk, "2.3.0", URL, SHA, 123)).unwrap();
        let cases: [(&str, serde_json::Value); 4] = [
            ("version", "9.9.9".into()),
            ("url", "https://evil.example/Lightify-Setup.exe".into()),
            ("sha256", "b".repeat(64).into()),
            ("size", 124.into()),
        ];
        for (field, value) in cases {
            let mut m = good.clone();
            m[field] = value;
            assert!(parse_and_verify(&m.to_string(), &pk, false).is_err(), "tampered {field} accepted");
        }
        // Signed, but not https: refused unless the test override allows it.
        let http = manifest(&sk, "2.3.0", "http://lightify.stream/x.exe", SHA, 123);
        assert!(parse_and_verify(&http, &pk, false).is_err());
        assert!(parse_and_verify(&http, &pk, true).is_ok());
    }

    #[tokio::test]
    async fn local_override_check_download_pending() {
        let (sk, pk) = test_key();
        let dir = temp_dir("e2e");
        let payload = b"pretend this is Lightify-Setup.exe".repeat(1000);
        let src = dir.join("served-setup.exe");
        std::fs::write(&src, &payload).unwrap();
        let sha = format!("{:x}", Sha256::digest(&payload));
        let text = manifest(&sk, "2.3.0", &src.to_string_lossy(), &sha, payload.len() as u64);
        let manifest_path = dir.join("latest.json");
        std::fs::write(&manifest_path, &text).unwrap();

        // The only test that touches this env var.
        std::env::set_var("LIGHTIFY_UPDATE_URL", &manifest_path);
        let found = check_with_key("2.2.4", &pk).await;
        let same = check_with_key("2.3.0", &pk).await;
        let real_key = check_with_key("2.2.4", &PUBLIC_KEY).await;
        std::env::remove_var("LIGHTIFY_UPDATE_URL");

        let rel = found.expect("check ok").expect("2.3.0 offered over 2.2.4");
        assert!(same.expect("check ok").is_none(), "same version is up to date");
        assert!(real_key.is_err(), "test-signed manifest must fail the real key");

        let updates = dir.join("updates");
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s2 = seen.clone();
        let path = download_into(&updates, &rel, move |d, t| {
            assert!(d <= t);
            s2.store(d, std::sync::atomic::Ordering::SeqCst);
        })
        .await
        .expect("download ok");
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), payload.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        assert!(updates.join(sidecar_name("2.3.0")).is_file());

        // Reuse: a second download doesn't need the source any more.
        std::fs::remove_file(&src).unwrap();
        assert_eq!(download_into(&updates, &rel, |_, _| {}).await.unwrap(), path);

        // Pending re-verifies from disk.
        let (v, p) = pending_in(&updates, "2.2.4", &pk).expect("pending");
        assert_eq!((v.as_str(), p.as_path()), ("2.3.0", path.as_path()));
        assert!(pending_in(&updates, "2.2.4", &PUBLIC_KEY).is_none(), "wrong key");
        // A corrupted file is not pending.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(pending_in(&updates, "2.2.4", &pk).is_none());
        // Once installed, the stale installer is cleaned away.
        assert!(pending_in(&updates, "2.3.0", &pk).is_none());
        assert!(!path.exists());

        // A tampered download is refused and leaves nothing behind.
        let bad = Release { sha256: "c".repeat(64), ..rel.clone() };
        std::fs::write(&src, &payload).unwrap();
        assert!(download_into(&updates, &bad, |_, _| {}).await.is_err());
        assert!(!updates.join(installer_name("2.3.0")).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
