//! DPAPI-encrypted refresh-token files for portable Windows builds, where nothing may land in
//! the user's profile (the Credential Manager included).
//!
//! One file per account, `<dir>\ytdata-<channel_id>.bin`, holding the raw `CryptProtectData`
//! blob (current user scope, `CRYPTPROTECT_UI_FORBIDDEN`, fixed entropy). The blob only opens
//! for the same Windows user on the same machine: carried to another one, `load` returns
//! [`AuthError::SecretUndecryptable`] ("connect again") and the file is left exactly as it was.
//! Only `delete` (disconnecting the account) removes a file, and only that account's.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

use super::store::{validate_account_id, TokenStore};
use crate::error::{AuthError, Error};

/// Secondary entropy mixed into every blob: a blob from any other DPAPI user on this account
/// does not decrypt as one of ours. Bump the version to invalidate every stored file.
const ENTROPY: &[u8] = b"LiMusicForge/ytdata/v1";

/// Refresh tokens as DPAPI-encrypted files in one directory (the app passes `<data>\secrets`).
#[derive(Debug, Clone)]
pub struct DpapiFileStore {
    dir: PathBuf,
    entropy: Vec<u8>,
}

impl DpapiFileStore {
    /// Store rooted at `dir`. Nothing is touched until the first `save` (which creates `dir`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), entropy: ENTROPY.to_vec() }
    }

    #[cfg(test)]
    fn with_entropy(dir: impl Into<PathBuf>, entropy: &[u8]) -> Self {
        Self { dir: dir.into(), entropy: entropy.to_vec() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<dir>\ytdata-<account_id>.bin`; rejects ids that are not plain channel ids.
    pub fn path_for(&self, account_id: &str) -> Result<PathBuf, Error> {
        validate_account_id(account_id)?;
        Ok(self.dir.join(format!("ytdata-{account_id}.bin")))
    }
}

impl TokenStore for DpapiFileStore {
    fn save(&self, account_id: &str, refresh_token: &str) -> Result<(), Error> {
        let path = self.path_for(account_id)?;
        let blob = protect(refresh_token.as_bytes(), &self.entropy)?;
        fs::create_dir_all(&self.dir)?;
        write_atomically(&path, &blob)?;
        Ok(())
    }

    fn load(&self, account_id: &str) -> Result<Option<String>, Error> {
        let path = self.path_for(account_id)?;
        let blob = match fs::read(&path) {
            Ok(blob) => blob,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let plain = match unprotect(&blob, &self.entropy) {
            Ok(plain) => plain,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "stored credential cannot be decrypted; keeping the file"
                );
                return Err(AuthError::SecretUndecryptable.into());
            }
        };
        match String::from_utf8(plain) {
            Ok(token) => Ok(Some(token)),
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "stored credential is not UTF-8; keeping the file"
                );
                Err(AuthError::SecretUndecryptable.into())
            }
        }
    }

    fn delete(&self, account_id: &str) -> Result<(), Error> {
        let path = self.path_for(account_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// Writes `<path>.tmp`, flushes it to disk, then renames it over `path` (replacing it), so a
/// crash leaves either the old file or the new one. The `.tmp` is removed on failure.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn input_blob(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, Error> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| AuthError::Store("credential too large to encrypt".to_string()))?;
    // DPAPI only reads input blobs; the `*mut` is the struct's field type.
    Ok(CRYPT_INTEGER_BLOB { cbData: len, pbData: bytes.as_ptr() as *mut u8 })
}

/// Copies a DPAPI output blob, wipes it when it held plaintext, and frees it with `LocalFree`.
///
/// # Safety
/// `blob` must be an output blob filled in by `CryptProtectData`/`CryptUnprotectData` (or left
/// zeroed), not yet freed.
unsafe fn take_output(blob: CRYPT_INTEGER_BLOB, wipe: bool) -> Vec<u8> {
    if blob.pbData.is_null() {
        return Vec::new();
    }
    let len = blob.cbData as usize;
    // SAFETY: DPAPI allocated `cbData` bytes at `pbData` (caller contract).
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, len) }.to_vec();
    if wipe {
        // SAFETY: same allocation, still owned by us.
        unsafe { std::ptr::write_bytes(blob.pbData, 0, len) };
    }
    // SAFETY: DPAPI output is allocated with LocalAlloc and must be released with LocalFree.
    let _ = unsafe { LocalFree(Some(HLOCAL(blob.pbData.cast()))) };
    bytes
}

fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, Error> {
    let input = input_blob(plain)?;
    let entropy = input_blob(entropy)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input and entropy point at live slices for the duration of the call; output is
    // a zeroed blob DPAPI fills in.
    let result = unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    // SAFETY: output was filled in by CryptProtectData (or is still zeroed on failure).
    let blob = unsafe { take_output(output, false) };
    result.map_err(|err| AuthError::Store(format!("CryptProtectData failed: {err}")))?;
    Ok(blob)
}

/// Decrypts a blob. The error is DPAPI's own message (wrong user/machine, wrong entropy,
/// corrupted data); it never contains the secret.
fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
    let input = input_blob(blob).map_err(|err| err.to_string())?;
    let entropy = input_blob(entropy).map_err(|err| err.to_string())?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    let result = unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    // SAFETY: output was filled in by CryptUnprotectData (or is still zeroed on failure).
    let plain = unsafe { take_output(output, true) };
    result.map_err(|err| format!("CryptUnprotectData failed: {err}"))?;
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "UCdpapiTest_1";

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn dpapi_round_trips_and_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = DpapiFileStore::new(&dir);

        assert_eq!(store.load(ID).unwrap(), None);
        store.save(ID, "refresh-token-1").unwrap();
        assert_eq!(store.load(ID).unwrap().as_deref(), Some("refresh-token-1"));
        store.save(ID, "refresh-token-2").unwrap();
        assert_eq!(store.load(ID).unwrap().as_deref(), Some("refresh-token-2"));

        // Exactly one file, no `.tmp` left behind.
        assert_eq!(files_in(&dir), vec![format!("ytdata-{ID}.bin")]);
    }

    #[test]
    fn dpapi_file_is_encrypted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DpapiFileStore::new(tmp.path());
        store.save(ID, "plaintext-refresh-token").unwrap();
        let bytes = fs::read(store.path_for(ID).unwrap()).unwrap();
        let needle = b"plaintext-refresh-token";
        assert!(!bytes.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn dpapi_wrong_entropy_is_undecryptable_and_keeps_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        DpapiFileStore::new(tmp.path()).save(ID, "refresh-token").unwrap();
        let path = tmp.path().join(format!("ytdata-{ID}.bin"));
        let before = fs::read(&path).unwrap();

        let other = DpapiFileStore::with_entropy(tmp.path(), b"LiMusicForge/ytdata/v0");
        let err = other.load(ID).unwrap_err();
        assert!(err.is_secret_undecryptable(), "{err}");
        assert!(err.needs_reauthorization());

        assert_eq!(fs::read(&path).unwrap(), before, "file must be left untouched");
        // The right entropy still opens it.
        let store = DpapiFileStore::new(tmp.path());
        assert_eq!(store.load(ID).unwrap().as_deref(), Some("refresh-token"));
    }

    #[test]
    fn dpapi_corrupted_file_is_undecryptable_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DpapiFileStore::new(tmp.path());
        let path = store.path_for(ID).unwrap();
        fs::write(&path, b"not a dpapi blob").unwrap();

        let err = store.load(ID).unwrap_err();
        assert!(err.is_secret_undecryptable(), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"not a dpapi blob");
    }

    #[test]
    fn dpapi_delete_removes_only_that_account() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DpapiFileStore::new(tmp.path());
        store.save(ID, "token-a").unwrap();
        store.save("UCother", "token-b").unwrap();
        fs::write(tmp.path().join("unrelated.txt"), b"keep").unwrap();

        store.delete(ID).unwrap();
        assert_eq!(store.load(ID).unwrap(), None);
        assert_eq!(store.load("UCother").unwrap().as_deref(), Some("token-b"));
        assert_eq!(files_in(tmp.path()), vec!["unrelated.txt", "ytdata-UCother.bin"]);
        // Idempotent.
        store.delete(ID).unwrap();
    }

    #[test]
    fn dpapi_rejects_invalid_account_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secrets");
        let store = DpapiFileStore::new(&dir);
        for bad in ["", "..", "../escape", "a\\b", "a/b", "a.b", "C:x", "UC 1"] {
            assert!(matches!(
                store.save(bad, "token"),
                Err(Error::Auth(AuthError::InvalidAccountId(_)))
            ));
            assert!(matches!(store.load(bad), Err(Error::Auth(AuthError::InvalidAccountId(_)))));
            assert!(matches!(store.delete(bad), Err(Error::Auth(AuthError::InvalidAccountId(_)))));
        }
        assert!(!dir.exists(), "nothing may be written for a rejected id");
        assert!(!tmp.path().join("escape").exists());
    }

    #[test]
    fn dpapi_failed_write_leaves_no_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the destination file should go makes the rename fail.
        let target = tmp.path().join("ytdata-UCx.bin");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inner"), b"x").unwrap();
        assert!(write_atomically(&target, b"data").is_err());
        assert!(!tmp.path().join("ytdata-UCx.bin.tmp").exists());
        assert!(target.join("inner").exists());
    }
}
