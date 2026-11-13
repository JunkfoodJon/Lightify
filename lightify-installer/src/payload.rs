//! The self-extracting payload appended to the setup stub.
//!
//! Layout of `Lightify-Setup-<ver>.exe`:
//!
//! ```text
//! [ stub exe (this crate's Lightify-Setup.exe) ]
//! [ zstd stream:  u32-LE header length | header JSON | file 1 bytes | file 2 bytes | ... ]
//! [ footer, 24 bytes:  u64-LE payload offset | u64-LE payload length | MAGIC ]
//! ```
//!
//! The header lists every file with its size and SHA-256, in stream order. LICENSE.md
//! is always first, so the wizard can show it after decompressing only a few KB.
//! The stub finds the payload by reading its own last 24 bytes; `uninstall.exe` is
//! the first `offset` bytes - the stub without a payload.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 8] = *b"LFYPAY\x01\x00";
pub const FOOTER_LEN: u64 = 24;
pub const LICENSE_NAME: &str = "LICENSE.md";

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    pub version: String,
    pub files: Vec<FileEntry>,
}

impl Header {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    pub fn has(&self, name: &str) -> bool {
        self.files.iter().any(|f| f.name.eq_ignore_ascii_case(name))
    }
}

/// A payload file name must be a plain file name: no directories, no `..`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0']) && name.len() < 128
}

/// Where the payload sits inside `exe`: `(offset, len)`, or None for a bare stub.
pub fn locate(exe: &Path) -> io::Result<Option<(u64, u64)>> {
    let mut f = File::open(exe)?;
    let total = f.metadata()?.len();
    if total < FOOTER_LEN {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(total - FOOTER_LEN))?;
    let mut foot = [0u8; FOOTER_LEN as usize];
    f.read_exact(&mut foot)?;
    if foot[16..24] != MAGIC {
        return Ok(None);
    }
    let offset = u64::from_le_bytes(foot[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(foot[8..16].try_into().unwrap());
    if offset.checked_add(len) != Some(total - FOOTER_LEN) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "payload footer does not match the file size"));
    }
    Ok(Some((offset, len)))
}

/// Length of the bare stub inside `exe` (the whole file when it has no payload).
pub fn stub_len(exe: &Path) -> io::Result<u64> {
    Ok(match locate(exe)? {
        Some((offset, _)) => offset,
        None => std::fs::metadata(exe)?.len(),
    })
}

/// Copy just the stub part of `exe` to `dest` (that is what `uninstall.exe` is).
pub fn write_stub(exe: &Path, dest: &Path) -> io::Result<u64> {
    let n = stub_len(exe)?;
    let mut src = File::open(exe)?.take(n);
    let mut out = File::create(dest)?;
    let copied = io::copy(&mut src, &mut out)?;
    out.flush()?;
    if copied != n {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short stub copy"));
    }
    Ok(n)
}

type Dec = zstd::stream::read::Decoder<'static, BufReader<Take<File>>>;

/// Streams the files out of a payload, in order.
pub struct PayloadReader {
    pub header: Header,
    dec: Dec,
    next: usize,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl PayloadReader {
    /// None when `exe` carries no payload.
    pub fn open(exe: &Path) -> io::Result<Option<Self>> {
        let Some((offset, len)) = locate(exe)? else { return Ok(None) };
        let mut f = File::open(exe)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut dec = zstd::stream::read::Decoder::new(f.take(len))?;
        let mut n = [0u8; 4];
        dec.read_exact(&mut n)?;
        let n = u32::from_le_bytes(n) as usize;
        if n > 1 << 20 {
            return Err(bad("payload header too large"));
        }
        let mut json = vec![0u8; n];
        dec.read_exact(&mut json)?;
        let header: Header = serde_json::from_slice(&json).map_err(|e| bad(format!("payload header: {e}")))?;
        if let Some(f) = header.files.iter().find(|f| !valid_name(&f.name)) {
            return Err(bad(format!("payload file name {:?}", f.name)));
        }
        Ok(Some(Self { header, dec, next: 0 }))
    }

    /// The next file's entry, if any (without consuming it).
    pub fn peek(&self) -> Option<&FileEntry> {
        self.header.files.get(self.next)
    }

    /// Stream the next file into `out`, calling `progress(n)` with each chunk's size,
    /// and check its SHA-256. None once every file has been read.
    pub fn next_file(&mut self, out: &mut dyn Write, progress: &mut dyn FnMut(u64)) -> io::Result<Option<FileEntry>> {
        let Some(entry) = self.header.files.get(self.next).cloned() else { return Ok(None) };
        self.next += 1;
        let mut left = entry.size;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        while left > 0 {
            let want = buf.len().min(left as usize);
            let got = self.dec.read(&mut buf[..want])?;
            if got == 0 {
                return Err(bad(format!("payload ends inside {}", entry.name)));
            }
            h.update(&buf[..got]);
            out.write_all(&buf[..got])?;
            left -= got as u64;
            progress(got as u64);
        }
        if crate::hex(&h.finalize()) != entry.sha256 {
            return Err(bad(format!("{} is corrupt (SHA-256 mismatch) - download Setup again", entry.name)));
        }
        Ok(Some(entry))
    }

    /// Read the next file into memory (used for LICENSE.md).
    pub fn next_to_vec(&mut self) -> io::Result<Option<(FileEntry, Vec<u8>)>> {
        let mut v = Vec::new();
        Ok(self.next_file(&mut v, &mut |_| {})?.map(|e| (e, v)))
    }
}

/// Unpack one file of `exe`'s payload to `dest`, checked like any other read. False when
/// the payload has no file of that name, or `exe` has no payload.
pub fn extract_file(exe: &Path, name: &str, dest: &Path) -> io::Result<bool> {
    let Some(mut r) = PayloadReader::open(exe)? else { return Ok(false) };
    while let Some(entry) = r.peek() {
        if entry.name.eq_ignore_ascii_case(name) {
            let mut out = File::create(dest)?;
            r.next_file(&mut out, &mut |_| {})?;
            out.flush()?;
            return Ok(true);
        }
        r.next_file(&mut io::sink(), &mut |_| {})?;
    }
    Ok(false)
}

fn sha256_of(path: &Path) -> io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((crate::hex(&h.finalize()), total))
}

/// Build `out` = bare stub of `stub` + payload of `files` (`(name, source)`).
/// LICENSE.md is moved to the front.
pub fn write_payload(
    stub: &Path,
    out: &Path,
    version: &str,
    files: &[(String, PathBuf)],
    level: i32,
    threads: u32,
) -> io::Result<Header> {
    let mut ordered: Vec<&(String, PathBuf)> = files.iter().collect();
    ordered.sort_by_key(|(n, _)| !n.eq_ignore_ascii_case(LICENSE_NAME));
    let mut entries = Vec::new();
    for (name, src) in &ordered {
        if !valid_name(name) {
            return Err(bad(format!("bad payload name {name:?}")));
        }
        let (sha256, size) = sha256_of(src)?;
        entries.push(FileEntry { name: name.clone(), size, sha256 });
    }
    let header = Header { version: version.to_string(), files: entries };
    let json = serde_json::to_vec(&header).map_err(|e| bad(e.to_string()))?;

    let offset = write_stub(stub, out)?;
    let file = std::fs::OpenOptions::new().append(true).open(out)?;
    let mut enc = zstd::stream::write::Encoder::new(file, level)?;
    if threads > 1 {
        enc.multithread(threads)?;
    }
    enc.include_checksum(true)?;
    enc.write_all(&(json.len() as u32).to_le_bytes())?;
    enc.write_all(&json)?;
    for ((_, src), entry) in ordered.iter().zip(&header.files) {
        let mut f = File::open(src)?;
        let n = io::copy(&mut f, &mut enc)?;
        if n != entry.size {
            return Err(bad(format!("{} changed while packing", src.display())));
        }
    }
    let mut file = enc.finish()?;
    file.flush()?;
    let end = file.seek(SeekFrom::End(0))?;
    let len = end - offset;
    file.write_all(&offset.to_le_bytes())?;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(&MAGIC)?;
    file.flush()?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lightify-payload-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_plain_files() {
        assert!(valid_name("Lightify.exe"));
        assert!(!valid_name("..\\evil.exe"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("C:x"));
        assert!(!valid_name(""));
        assert!(!valid_name(".."));
    }

    #[test]
    fn roundtrip_and_stub_extraction() {
        let d = tmp("rt");
        let stub = d.join("stub.exe");
        std::fs::write(&stub, b"MZ pretend stub".repeat(100)).unwrap();
        assert!(locate(&stub).unwrap().is_none());
        let app = d.join("Lightify.exe");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&app, &big).unwrap();
        let lic = d.join("LICENSE.md");
        std::fs::write(&lic, b"# Licence\n\nBe nice.\n").unwrap();

        let out = d.join("setup.exe");
        let files = vec![("Lightify.exe".to_string(), app.clone()), ("LICENSE.md".to_string(), lic.clone())];
        let header = write_payload(&stub, &out, "2.3.0", &files, 3, 2).unwrap();
        assert_eq!(header.files[0].name, "LICENSE.md", "licence goes first");

        let (offset, _) = locate(&out).unwrap().expect("payload found");
        assert_eq!(offset, std::fs::metadata(&stub).unwrap().len());

        let mut r = PayloadReader::open(&out).unwrap().unwrap();
        assert_eq!(r.header, header);
        let (e, text) = r.next_to_vec().unwrap().unwrap();
        assert_eq!(e.name, "LICENSE.md");
        assert_eq!(text, b"# Licence\n\nBe nice.\n");
        let mut got = Vec::new();
        let mut seen = 0u64;
        let e = r.next_file(&mut got, &mut |n| seen += n).unwrap().unwrap();
        assert_eq!(e.name, "Lightify.exe");
        assert_eq!(seen, big.len() as u64);
        assert_eq!(got, big);
        assert!(r.next_file(&mut got, &mut |_| {}).unwrap().is_none());

        // uninstall.exe = the stub alone; re-packing a packed exe uses only its stub.
        let un = d.join("uninstall.exe");
        write_stub(&out, &un).unwrap();
        assert_eq!(std::fs::read(&un).unwrap(), std::fs::read(&stub).unwrap());
        let out2 = d.join("setup2.exe");
        write_payload(&out, &out2, "2.3.1", &files, 3, 1).unwrap();
        assert_eq!(locate(&out2).unwrap().unwrap().0, offset);

        // One file comes out on its own (what stage-release.ps1 reads the app's version from).
        let got = d.join("got.exe");
        assert!(extract_file(&out, "lightify.exe", &got).unwrap());
        assert_eq!(std::fs::read(&got).unwrap(), big);
        assert!(!extract_file(&out, "nope.exe", &got).unwrap());
        assert!(!extract_file(&stub, "Lightify.exe", &got).unwrap());

        // A flipped byte inside the payload is caught.
        let mut bytes = std::fs::read(&out).unwrap();
        let mid = offset as usize + (bytes.len() - offset as usize) / 2;
        bytes[mid] ^= 0x55;
        let bad_path = d.join("bad.exe");
        std::fs::write(&bad_path, &bytes).unwrap();
        let res = (|| -> io::Result<()> {
            let mut r = PayloadReader::open(&bad_path)?.unwrap();
            while r.next_file(&mut io::sink(), &mut |_| {})?.is_some() {}
            Ok(())
        })();
        assert!(res.is_err(), "corruption must be detected");
        let _ = std::fs::remove_dir_all(&d);
    }
}
