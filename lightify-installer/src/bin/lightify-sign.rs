//! lightify-sign: the release manifest (latest.json) the app's updater trusts.
//!
//!     lightify-sign keygen [--key PATH] [--force]
//!     lightify-sign pubkey [--key PATH]
//!     lightify-sign sign <installer.exe> <version> <url> [--key PATH] [--out PATH]
//!     lightify-sign verify <latest.json> [--installer PATH] [--pubkey HEX]
//!
//! The private key lives OUTSIDE the repo, by default at
//! `%USERPROFILE%\.lightify-release\update-signing.key` (override: --key or
//! LIGHTIFY_SIGNING_KEY). Its public half is compiled into lightify-core/src/update.rs
//! (`PUBLIC_KEY`); a manifest signed with any other key is rejected by every installed
//! copy of the app. Losing the key means shipping one release by hand-download with a
//! new public key; never regenerate it casually (`keygen` refuses to overwrite).
//!
//! `sign` writes `latest.json` next to the installer unless --out says otherwise:
//!     {"version","url","sha256","size","sig"}
//! with `sig` = base64 ed25519 over `lightify-update\n{version}\n{url}\n{sha256}\n{size}`.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use lightify_installer::{hex, signed_message};
use sha2::{Digest, Sha256};

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("lightify-sign: {msg}");
    std::process::exit(1);
}

fn default_key_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LIGHTIFY_SIGNING_KEY").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| die("no home directory"))
        .join(".lightify-release")
        .join("update-signing.key")
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn load_key(path: &Path) -> SigningKey {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| die(format!("{}: no key line", path.display())));
    let seed: [u8; 32] = decode_hex(line)
        .and_then(|v| v.try_into().ok())
        .unwrap_or_else(|| die(format!("{}: expected 64 hex digits", path.display())));
    SigningKey::from_bytes(&seed)
}

fn sha256_file(path: &Path) -> (String, u64) {
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf).unwrap_or_else(|e| die(e));
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        n_total += n as u64;
    }
    (hex(&h.finalize()), n_total)
}

fn rust_array(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", ")
}

/// Pull `--name value` out of `args`.
fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        die(format!("{name} needs a value"));
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    version: String,
    url: String,
    sha256: String,
    size: u64,
    sig: String,
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        die("usage: lightify-sign keygen|pubkey|sign|verify ... (see the source header)");
    }
    let cmd = args.remove(0);
    match cmd.as_str() {
        "keygen" => {
            let path = take_opt(&mut args, "--key").map(PathBuf::from).unwrap_or_else(default_key_path);
            let force = take_flag(&mut args, "--force");
            if path.exists() && !force {
                die(format!(
                    "{} already exists - refusing to replace the release key (every installed app trusts it). \
                     Pass --force only if you really mean to rotate it.",
                    path.display()
                ));
            }
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).unwrap_or_else(|e| die(format!("no OS randomness: {e}")));
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).unwrap_or_else(|e| die(format!("{}: {e}", dir.display())));
            }
            let body = format!(
                "# Lightify update-signing key (ed25519 seed, hex). Keep private; never commit.\n{}\n",
                hex(&seed)
            );
            std::fs::write(&path, body).unwrap_or_else(|e| die(format!("{}: {e}", path.display())));
            let pk = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            println!("wrote private key: {}", path.display());
            println!("public key hex:    {}", hex(&pk));
            println!("lightify-core/src/update.rs PUBLIC_KEY = [{}];", rust_array(&pk));
        }
        "pubkey" => {
            let path = take_opt(&mut args, "--key").map(PathBuf::from).unwrap_or_else(default_key_path);
            println!("{}", hex(&load_key(&path).verifying_key().to_bytes()));
        }
        "sign" => {
            let key_path = take_opt(&mut args, "--key").map(PathBuf::from).unwrap_or_else(default_key_path);
            let out = take_opt(&mut args, "--out").map(PathBuf::from);
            let [installer, version, url] = <[String; 3]>::try_from(args)
                .unwrap_or_else(|_| die("usage: lightify-sign sign <installer.exe> <version> <url> [--key PATH] [--out PATH]"));
            let installer = PathBuf::from(installer);
            if !url.starts_with("https://") {
                die("the url must be https:// (installed apps refuse anything else)");
            }
            let key = load_key(&key_path);
            let (sha256, size) = sha256_file(&installer);
            let sig = key.sign(signed_message(&version, &url, &sha256, size).as_bytes());
            let m = Manifest {
                version,
                url,
                sha256,
                size,
                sig: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
            };
            let out = out.unwrap_or_else(|| installer.parent().unwrap_or(Path::new(".")).join("latest.json"));
            let json = serde_json::to_string_pretty(&m).unwrap();
            std::fs::write(&out, json + "\n").unwrap_or_else(|e| die(format!("{}: {e}", out.display())));
            println!("signed {} {} ({} bytes)", m.version, m.sha256, m.size);
            println!("public key {}", hex(&key.verifying_key().to_bytes()));
            println!("wrote {}", out.display());
        }
        "verify" => {
            let installer = take_opt(&mut args, "--installer").map(PathBuf::from);
            let pubkey = take_opt(&mut args, "--pubkey");
            let [manifest_path] = <[String; 1]>::try_from(args)
                .unwrap_or_else(|_| die("usage: lightify-sign verify <latest.json> [--installer PATH] [--pubkey HEX]"));
            let text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| die(format!("{manifest_path}: {e}")));
            let m: Manifest = serde_json::from_str(&text).unwrap_or_else(|e| die(format!("{manifest_path}: {e}")));
            let pk: [u8; 32] = match pubkey {
                Some(h) => decode_hex(&h).and_then(|v| v.try_into().ok()).unwrap_or_else(|| die("--pubkey: 64 hex digits")),
                None => load_key(&default_key_path()).verifying_key().to_bytes(),
            };
            let vk = VerifyingKey::from_bytes(&pk).unwrap_or_else(|_| die("bad public key"));
            let sig: [u8; 64] = base64::engine::general_purpose::STANDARD
                .decode(m.sig.trim())
                .ok()
                .or_else(|| decode_hex(&m.sig))
                .and_then(|v| v.try_into().ok())
                .unwrap_or_else(|| die("sig is not 64 bytes"));
            vk.verify_strict(signed_message(&m.version, &m.url, &m.sha256, m.size).as_bytes(), &Signature::from_bytes(&sig))
                .unwrap_or_else(|_| die("SIGNATURE CHECK FAILED"));
            if let Some(inst) = installer {
                let (sha, size) = sha256_file(&inst);
                if sha != m.sha256 || size != m.size {
                    die(format!("{} does not match the manifest (sha256/size)", inst.display()));
                }
            }
            println!("ok: {} {} signed by {}", m.version, m.url, hex(&pk));
        }
        other => die(format!("unknown command {other:?}")),
    }
}
