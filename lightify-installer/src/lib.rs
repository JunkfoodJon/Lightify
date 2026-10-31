//! Pieces shared by the setup stub (`Lightify-Setup`) and the release tools
//! (`lightify-pack`, `lightify-sign`).

pub mod payload;

/// The exact bytes a release manifest's signature covers. lightify-core/src/update.rs
/// verifies the same string; both crates carry the same test vector so they cannot
/// drift apart silently.
pub fn signed_message(version: &str, url: &str, sha256: &str, size: u64) -> String {
    format!("lightify-update\n{version}\n{url}\n{sha256}\n{size}")
}

/// Hex-encode bytes (lowercase).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Same vector as lightify-core's `update::tests::signature_test_vector`.
    #[test]
    fn signature_test_vector_matches_core() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let sha = "a".repeat(64);
        let url = "https://lightify.stream/downloads/Lightify_2.3.0_x64-setup.exe";
        let sig = sk.sign(signed_message("2.3.0", url, &sha, 123).as_bytes());
        assert_eq!(
            hex(&sig.to_bytes()),
            "138266a7b3a876bd71b65d078693087369ed858a3ca8821818a8f9f3915470058c985fb978e8f238bd26747473265eaedcdb9de9913203ad442315cd56b9d203"
        );
    }
}
