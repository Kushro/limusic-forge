//! Sealing the [`SecretsBundle`] into `secrets.bin` with a passphrase.
//!
//! The format is `age` in passphrase mode (scrypt + ChaCha20-Poly1305), so nothing here invents a
//! KDF or an AEAD; this file only feeds it JSON and maps its failures to the two the UI tells
//! apart: a wrong passphrase (the header was fine but nothing unlocked it) and a damaged file.
//! The JSON that exists between the bundle and the cipher is wiped as soon as it is not needed;
//! the bundle itself wipes its strings on drop (`profile/mod.rs`).
//!
//! scrypt is deliberately slow (about a second per call at age's default work factor), so
//! callers run this off the UI thread and tests keep the number of calls small.

use std::io::{Read, Write};

use age::secrecy::SecretString;
use zeroize::Zeroizing;

use super::{ProfileError, Result, SecretsBundle, MIN_PASSPHRASE_CHARS};

/// Rejects a passphrase shorter than [`MIN_PASSPHRASE_CHARS`] characters (chars, not bytes: ten
/// `ñ` are fine).
pub fn check_passphrase(pass: &str) -> Result<()> {
    if pass.chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(ProfileError::PassphraseTooShort);
    }
    Ok(())
}

/// The bundle as JSON, encrypted to `pass`. Same checks as [`check_passphrase`].
pub fn seal(bundle: &SecretsBundle, pass: &str) -> Result<Vec<u8>> {
    check_passphrase(pass)?;
    let json = Zeroizing::new(
        serde_json::to_vec(bundle).map_err(|e| ProfileError::Other(format!("secrets: {e}")))?,
    );
    let encryptor = age::Encryptor::with_user_passphrase(SecretString::from(pass.to_owned()));
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| ProfileError::Other(format!("encrypt: {e}")))?;
    writer.write_all(&json)?;
    writer.finish()?;
    Ok(out)
}

/// Decrypts `secrets.bin`. [`ProfileError::WrongPassphrase`] when the passphrase does not unlock
/// it, [`ProfileError::CorruptSecrets`] when the bytes are not an age file, were tampered with, or
/// do not hold a bundle.
pub fn unseal(bytes: &[u8], pass: &str) -> Result<SecretsBundle> {
    let decryptor = age::Decryptor::new(bytes).map_err(|_| ProfileError::CorruptSecrets)?;
    let identity = age::scrypt::Identity::new(SecretString::from(pass.to_owned()));
    let mut reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity)).map_err(
        |e| match e {
            age::DecryptError::DecryptionFailed => ProfileError::WrongPassphrase,
            _ => ProfileError::CorruptSecrets,
        },
    )?;
    let mut json = Zeroizing::new(Vec::new());
    // The payload is authenticated chunk by chunk: a flipped bit surfaces here as an io error.
    reader.read_to_end(&mut json).map_err(|_| ProfileError::CorruptSecrets)?;
    serde_json::from_slice(&json).map_err(|_| ProfileError::CorruptSecrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::GoogleSession;

    const PASS: &str = "correct horse battery";

    fn bundle() -> SecretsBundle {
        // Field by field: SecretsBundle implements Drop, so `..Default::default()` cannot move out.
        let mut b = SecretsBundle::default();
        b.active_session_cookie = Some("SAPISID=fake-active".into());
        b.google_sessions = vec![GoogleSession {
            account_id: "acc-1".into(),
            session_cookie: "SAPISID=fake-one".into(),
        }];
        b.client_secret_json = Some(r#"{"installed":{"client_id":"FAKE"}}"#.into());
        b.lastfm_session_key = Some("fake-lastfm".into());
        b.ytdata_refresh_tokens.insert("UCfake0000000000000001".into(), "fake-refresh".into());
        b
    }

    #[test]
    fn profile_crypto_passphrase_length_counts_chars() {
        assert!(matches!(check_passphrase("short"), Err(ProfileError::PassphraseTooShort)));
        assert!(matches!(check_passphrase(&"ñ".repeat(9)), Err(ProfileError::PassphraseTooShort)));
        check_passphrase(&"ñ".repeat(10)).unwrap();
        check_passphrase("0123456789").unwrap();
        // seal enforces it too, before any scrypt work.
        assert!(matches!(seal(&bundle(), "tiny"), Err(ProfileError::PassphraseTooShort)));
    }

    #[test]
    fn profile_crypto_round_trip_wrong_passphrase_and_tamper() {
        let b = bundle();
        let sealed = seal(&b, PASS).unwrap();
        let text = String::from_utf8_lossy(&sealed);
        assert!(!text.contains("fake-refresh") && !text.contains("SAPISID"), "plaintext leaked");

        assert_eq!(unseal(&sealed, PASS).unwrap(), b);
        assert!(matches!(
            unseal(&sealed, "another long passphrase"),
            Err(ProfileError::WrongPassphrase)
        ));

        // A bit flipped in the encrypted payload (the end of the file) is detected.
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(unseal(&tampered, PASS), Err(ProfileError::CorruptSecrets)));
    }

    #[test]
    fn profile_crypto_garbage_is_corrupt() {
        assert!(matches!(unseal(b"", PASS), Err(ProfileError::CorruptSecrets)));
        assert!(matches!(
            unseal(b"definitely not an age file", PASS),
            Err(ProfileError::CorruptSecrets)
        ));
    }
}
