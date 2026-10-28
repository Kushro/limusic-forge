//! The `.lmfprofile` zip: writing it atomically and reading it back without trusting it.
//!
//! A profile file comes from another machine (or from a stranger), so extraction treats every
//! entry name as hostile: absolute paths, `..`, drive prefixes and backslashes are refused
//! (zip-slip), only the entries listed in `profile/mod.rs` are read, and every entry has a size
//! cap so a small file cannot expand into a disk-filling bomb. The writer goes to
//! `<dest>.partial`, fsyncs and renames, so a crash never leaves a half-written profile under
//! the real name; on unix the file is created `0600` because it may hold `secrets.bin`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{
    Manifest, ProfileError, Result, WebStorage, DB_ENTRY, FILES_PREFIX, FORMAT_VERSION,
    MANIFEST_ENTRY, SECRETS_ENTRY, WEBSTORAGE_ENTRY, WEBSTORAGE_KEYS,
};

/// Largest `profile.sqlite` accepted on extraction.
const MAX_DB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest of every other entry (manifest, webstorage, secrets, each copied file).
const MAX_OTHER_BYTES: u64 = 64 * 1024 * 1024;

/// Everything that goes into the zip.
pub struct ArchiveInput<'a> {
    pub manifest: &'a Manifest,
    pub db_file: &'a Path,
    pub webstorage: &'a WebStorage,
    /// (path relative to the data dir, using '/'; absolute source file) → stored as `files/<rel>`.
    pub files: &'a [(String, PathBuf)],
    /// The sealed `secrets.bin`, when the user opted in.
    pub secrets: Option<&'a [u8]>,
}

/// What [`extract`] leaves in the target directory.
#[derive(Debug)]
pub struct Extracted {
    pub manifest: Manifest,
    pub db_file: PathBuf,
    pub webstorage: WebStorage,
    /// `<into>/files`; may not exist when the archive carried no files.
    pub files_dir: PathBuf,
    pub secrets: Option<Vec<u8>>,
}

fn zip_err(e: ZipError) -> ProfileError {
    match e {
        ZipError::Io(e) => ProfileError::Io(e),
        ZipError::InvalidArchive(m) => ProfileError::BadFormat(m.into_owned()),
        other => ProfileError::Zip(other.to_string()),
    }
}

/// Whether `name` is a plain relative path with '/' separators and no way to climb out.
fn is_safe_entry_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.contains(['\\', ':', '\0']) {
        return false;
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    !body.is_empty() && body.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".partial");
    dest.with_file_name(name)
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Writes the archive to `<dest>.partial`, fsyncs, then renames over `dest` (an existing file is
/// replaced). On unix the file is `0600`.
pub fn write(dest: &Path, input: &ArchiveInput) -> Result<()> {
    let partial = partial_path(dest);
    let result = write_partial(&partial, input).and_then(|()| {
        fs::rename(&partial, dest)?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_partial(partial: &Path, input: &ArchiveInput) -> Result<()> {
    if let Some(parent) = partial.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let mut zip = ZipWriter::new(create_private(partial)?);

    let manifest = serde_json::to_vec_pretty(input.manifest)
        .map_err(|e| ProfileError::Other(format!("manifest: {e}")))?;
    zip.start_file(MANIFEST_ENTRY, deflated).map_err(zip_err)?;
    zip.write_all(&manifest)?;

    zip.start_file(DB_ENTRY, deflated.large_file(true)).map_err(zip_err)?;
    io::copy(&mut File::open(input.db_file)?, &mut zip)?;

    let web = serde_json::to_vec(input.webstorage)
        .map_err(|e| ProfileError::Other(format!("webstorage: {e}")))?;
    zip.start_file(WEBSTORAGE_ENTRY, deflated).map_err(zip_err)?;
    zip.write_all(&web)?;

    for (rel, src) in input.files {
        if !is_safe_entry_name(rel) || rel.ends_with('/') {
            return Err(ProfileError::Other(format!("unsafe file path {rel:?}")));
        }
        zip.start_file(format!("{FILES_PREFIX}{rel}"), deflated).map_err(zip_err)?;
        io::copy(&mut File::open(src)?, &mut zip)?;
    }

    // Already age-encrypted: compressing it would only cost time.
    if let Some(secrets) = input.secrets {
        zip.start_file(SECRETS_ENTRY, stored).map_err(zip_err)?;
        zip.write_all(secrets)?;
    }

    let file = zip.finish().map_err(zip_err)?;
    file.sync_all()?;
    Ok(())
}

/// Reads at most `cap` bytes of `r`; more than that is a bomb.
fn read_capped(r: &mut impl Read, cap: u64, what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(ProfileError::BadFormat(format!("{what} is too large")));
    }
    Ok(buf)
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ProfileError::BadFormat(format!("manifest.json: {e}")))?;
    // A newer file may not even fit today's struct: judge its version first.
    let version = value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ProfileError::BadFormat("manifest.json has no format_version".into()))?;
    if version > u64::from(FORMAT_VERSION) {
        return Err(ProfileError::TooNew(u32::try_from(version).unwrap_or(u32::MAX)));
    }
    serde_json::from_value(value)
        .map_err(|e| ProfileError::BadFormat(format!("manifest.json: {e}")))
}

fn manifest_of(archive: &mut ZipArchive<File>) -> Result<Manifest> {
    let bytes = match archive.by_name(MANIFEST_ENTRY) {
        Ok(mut entry) => read_capped(&mut entry, MAX_OTHER_BYTES, MANIFEST_ENTRY)?,
        Err(ZipError::FileNotFound) => {
            return Err(ProfileError::BadFormat("manifest.json is missing".into()))
        }
        Err(e) => return Err(zip_err(e)),
    };
    parse_manifest(&bytes)
}

/// The manifest only (import preview, no passphrase needed). `BadFormat` when the file is not a
/// profile, `TooNew` when its `format_version` is above [`FORMAT_VERSION`].
pub fn read_manifest(src: &Path) -> Result<Manifest> {
    let mut archive = ZipArchive::new(File::open(src)?).map_err(zip_err)?;
    manifest_of(&mut archive)
}

fn copy_capped(r: &mut impl Read, dest: &Path, cap: u64, what: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(dest)?;
    let copied = io::copy(&mut r.take(cap + 1), &mut out)?;
    if copied > cap {
        drop(out);
        let _ = fs::remove_file(dest);
        return Err(ProfileError::BadFormat(format!("{what} is too large")));
    }
    out.sync_all()?;
    Ok(())
}

/// Extracts into `into` (created if missing; expected empty). Refuses any entry whose name could
/// leave `into`, ignores names it does not know, caps every entry's size and keeps only
/// [`WEBSTORAGE_KEYS`] of the webstorage.
pub fn extract(src: &Path, into: &Path) -> Result<Extracted> {
    let mut archive = ZipArchive::new(File::open(src)?).map_err(zip_err)?;
    // Version first: do not unpack gigabytes of something this build cannot read.
    let manifest = manifest_of(&mut archive)?;
    fs::create_dir_all(into)?;

    let mut db_file = None;
    let mut webstorage = WebStorage::new();
    let mut secrets = None;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        let name = entry.name().to_string();
        if !is_safe_entry_name(&name) || entry.enclosed_name().is_none() {
            return Err(ProfileError::BadFormat(format!("unsafe entry name {name:?}")));
        }
        if entry.is_dir() {
            continue;
        }
        let declared = entry.size();
        match name.as_str() {
            MANIFEST_ENTRY => {}
            DB_ENTRY => {
                if declared > MAX_DB_BYTES {
                    return Err(ProfileError::BadFormat(format!("{name} is too large")));
                }
                let path = into.join(DB_ENTRY);
                copy_capped(&mut entry, &path, MAX_DB_BYTES, &name)?;
                db_file = Some(path);
            }
            WEBSTORAGE_ENTRY => {
                let bytes = read_capped(&mut entry, MAX_OTHER_BYTES, &name)?;
                let all: BTreeMap<String, String> = serde_json::from_slice(&bytes)
                    .map_err(|e| ProfileError::BadFormat(format!("{name}: {e}")))?;
                webstorage = all
                    .into_iter()
                    .filter(|(k, _)| WEBSTORAGE_KEYS.contains(&k.as_str()))
                    .collect();
            }
            SECRETS_ENTRY => {
                secrets = Some(read_capped(&mut entry, MAX_OTHER_BYTES, &name)?);
            }
            other => {
                let Some(rel) = other.strip_prefix(FILES_PREFIX).filter(|r| !r.is_empty()) else {
                    continue; // unknown entry: not ours, not extracted
                };
                let mut path = into.join("files");
                path.extend(rel.split('/'));
                copy_capped(&mut entry, &path, MAX_OTHER_BYTES, other)?;
            }
        }
    }

    let db_file =
        db_file.ok_or_else(|| ProfileError::BadFormat("profile.sqlite is missing".into()))?;
    Ok(Extracted { manifest, db_file, webstorage, files_dir: into.join("files"), secrets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Counts, SourceOs};

    fn manifest() -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            app_version: "1.4.0-forge.4".into(),
            source_os: SourceOs::Windows,
            source_data_dir: r"C:\Users\x\AppData\Roaming\LiMusicForge".into(),
            created_at: "2026-10-28T00:00:00Z".into(),
            includes_secrets: true,
            includes_caches: false,
            counts: Counts { settings: 3, files: 1, ..Counts::default() },
            secret_counts: None,
        }
    }

    /// A zip with the given (name, bytes) entries, written by hand so tests can forge bad ones.
    fn forge(path: &Path, entries: &[(&str, &str)]) {
        let mut zip = ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn manifest_json() -> String {
        serde_json::to_string(&manifest()).unwrap()
    }

    #[test]
    fn profile_archive_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("snap.sqlite");
        fs::write(&db, b"SQLite format 3\0fake").unwrap();
        let cover = dir.path().join("c.jpg");
        fs::write(&cover, b"jpeg-bytes").unwrap();
        let mut web = WebStorage::new();
        web.insert("zoom".into(), "1.2".into());
        web.insert("not-allowed-key".into(), "x".into());
        let m = manifest();
        let files = vec![("covers/ab/c.jpg".to_string(), cover.clone())];
        let dest = dir.path().join("out.lmfprofile");
        fs::write(&dest, b"old contents").unwrap();

        write(
            &dest,
            &ArchiveInput {
                manifest: &m,
                db_file: &db,
                webstorage: &web,
                files: &files,
                secrets: Some(b"sealed-bytes"),
            },
        )
        .unwrap();
        assert!(!partial_path(&dest).exists(), "the .partial is renamed away");
        assert_eq!(read_manifest(&dest).unwrap(), m);

        let into = dir.path().join("x");
        let got = extract(&dest, &into).unwrap();
        assert_eq!(got.manifest, m);
        assert_eq!(fs::read(&got.db_file).unwrap(), b"SQLite format 3\0fake");
        assert_eq!(got.webstorage.len(), 1, "only WEBSTORAGE_KEYS survive");
        assert_eq!(got.webstorage["zoom"], "1.2");
        assert_eq!(got.secrets.as_deref(), Some(&b"sealed-bytes"[..]));
        assert_eq!(
            fs::read(got.files_dir.join("covers").join("ab").join("c.jpg")).unwrap(),
            b"jpeg-bytes"
        );

        // No secrets, no files: the optional parts come back empty.
        let dest2 = dir.path().join("plain.lmfprofile");
        write(
            &dest2,
            &ArchiveInput {
                manifest: &m,
                db_file: &db,
                webstorage: &WebStorage::new(),
                files: &[],
                secrets: None,
            },
        )
        .unwrap();
        let got = extract(&dest2, &dir.path().join("y")).unwrap();
        assert!(got.secrets.is_none() && !got.files_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn profile_archive_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.sqlite");
        fs::write(&db, b"x").unwrap();
        let dest = dir.path().join("o.lmfprofile");
        let m = manifest();
        write(
            &dest,
            &ArchiveInput {
                manifest: &m,
                db_file: &db,
                webstorage: &WebStorage::new(),
                files: &[],
                secrets: None,
            },
        )
        .unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn profile_archive_rejects_zip_slip_entries() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest_json();
        let m = m.as_str();
        let bad_names = [
            "../evil.txt",
            "files/../../evil.txt",
            "/abs/evil.txt",
            "files/..\\evil.txt",
            "files\\evil.txt",
            "C:/evil.txt",
            "files/C:evil.txt",
        ];
        for (i, bad) in bad_names.iter().enumerate() {
            let zip = dir.path().join(format!("bad{i}.lmfprofile"));
            forge(&zip, &[(MANIFEST_ENTRY, m), (DB_ENTRY, "db"), (*bad, "pwned")]);
            let into = dir.path().join(format!("into{i}"));
            let err = extract(&zip, &into).unwrap_err();
            assert!(matches!(err, ProfileError::BadFormat(_)), "{bad}: {err}");
            assert!(!dir.path().join("evil.txt").exists());
        }
        assert!(!is_safe_entry_name(""));
        assert!(is_safe_entry_name("files/covers/a.jpg"));
    }

    #[test]
    fn profile_archive_too_new_and_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();

        let newer = dir.path().join("new.lmfprofile");
        // Fields this build does not know: it must still say "too new", not "bad format".
        let json = format!(r#"{{"format_version": {}, "brand_new": true}}"#, FORMAT_VERSION + 1);
        forge(&newer, &[(MANIFEST_ENTRY, json.as_str()), (DB_ENTRY, "db")]);
        let err = read_manifest(&newer).unwrap_err();
        assert!(matches!(err, ProfileError::TooNew(v) if v == FORMAT_VERSION + 1), "{err}");
        assert_eq!(err.code(), "too_new");
        assert!(matches!(
            extract(&newer, &dir.path().join("a")).unwrap_err(),
            ProfileError::TooNew(_)
        ));

        let none = dir.path().join("none.lmfprofile");
        forge(&none, &[(DB_ENTRY, "db")]);
        assert!(matches!(read_manifest(&none).unwrap_err(), ProfileError::BadFormat(_)));

        let nodb = dir.path().join("nodb.lmfprofile");
        forge(&nodb, &[(MANIFEST_ENTRY, manifest_json().as_str())]);
        assert!(matches!(
            extract(&nodb, &dir.path().join("b")).unwrap_err(),
            ProfileError::BadFormat(_)
        ));

        let junk = dir.path().join("junk.lmfprofile");
        fs::write(&junk, b"this is not a zip at all").unwrap();
        assert!(matches!(read_manifest(&junk).unwrap_err(), ProfileError::BadFormat(_)));
    }

    #[test]
    fn profile_archive_ignores_unknown_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("u.lmfprofile");
        forge(
            &zip,
            &[(MANIFEST_ENTRY, manifest_json().as_str()), (DB_ENTRY, "db"), ("notes.txt", "hi")],
        );
        let into = dir.path().join("o");
        extract(&zip, &into).unwrap();
        assert!(!into.join("notes.txt").exists());
    }
}
