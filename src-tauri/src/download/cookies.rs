//! The signed-in session's cookies as a Netscape `cookies.txt`, for yt-dlp's `--cookies`.
//!
//! One file per run, `<data>/tmp/cookies-<16 hex>.txt`, created new (never over an existing file)
//! and owner-only on Unix. [`CookieFile`] deletes exactly that path when dropped, so the file
//! lives as long as the yt-dlp process that reads it. Its contents are the login credential:
//! nothing here logs them, or the path. A crash can leave one behind; [`sweep_stale`] removes
//! those at the next start, matching only this module's own file names.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The domain every cookie is filed under. The session's cookies are YouTube's (`SAPISID`,
/// `__Secure-3PSID`, …), which yt-dlp sends to www.youtube.com.
const DOMAIN: &str = ".youtube.com";
/// How long the written cookies claim to last. The file is gone long before; an expiry of 0
/// would mark them session cookies, which a cookie jar may drop on load.
const LIFETIME_SECS: i64 = 24 * 3600;
const PREFIX: &str = "cookies-";
const SUFFIX: &str = ".txt";

/// A `Cookie:` header (`a=1; b=2`) as a Netscape cookies file. Pairs without a name, or with a
/// tab or line break in them (which would break the format), are left out.
pub fn netscape(header: &str, now: i64) -> String {
    let expires = now + LIFETIME_SECS;
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for pair in header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else { continue };
        let name = name.trim();
        if name.is_empty() || [name, value].iter().any(|s| s.contains(['\t', '\n', '\r'])) {
            continue;
        }
        out.push_str(&format!("{DOMAIN}\tTRUE\t/\tTRUE\t{expires}\t{name}\t{value}\n"));
    }
    out
}

/// A cookies file on disk, deleted when this is dropped.
pub struct CookieFile {
    path: PathBuf,
}

impl CookieFile {
    /// Writes `header` into a new file in `dir` (created if needed). `None` for a header with no
    /// usable cookie in it; nothing is written then.
    pub fn write(dir: &Path, header: &str, now: i64) -> std::io::Result<Option<Self>> {
        let body = netscape(header, now);
        if !body.contains('\t') {
            return Ok(None);
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{PREFIX}{:016x}{SUFFIX}", rand::random::<u64>()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        // Owned from here on: a failed write still deletes the half-written file on drop.
        let guard = CookieFile { path };
        file.write_all(body.as_bytes())?;
        file.flush()?;
        Ok(Some(guard))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        // The exact file `write` created. No logging: the path is not the user's business in a
        // log, and a failure here has nothing to add.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether `name` is one of ours: `cookies-` + 16 lowercase hex digits + `.txt`.
fn is_ours(name: &str) -> bool {
    name.strip_prefix(PREFIX).and_then(|rest| rest.strip_suffix(SUFFIX)).is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Deletes the cookies files a crashed run left in `dir`: regular files named like the ones
/// [`CookieFile::write`] creates, directly in `dir`, nothing else. Answers how many.
pub fn sweep_stale(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        let ours = entry.file_name().to_str().is_some_and(is_ours);
        if is_file && ours && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_header_becomes_netscape_lines() {
        let text = netscape("SAPISID=abc/def; __Secure-3PSID=x=y=z;  HSID=h ", 1_000);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("# Netscape HTTP Cookie File"));
        assert_eq!(lines.next(), Some(".youtube.com\tTRUE\t/\tTRUE\t87400\tSAPISID\tabc/def"));
        assert_eq!(
            lines.next(),
            Some(".youtube.com\tTRUE\t/\tTRUE\t87400\t__Secure-3PSID\tx=y=z"),
            "only the first = splits"
        );
        assert_eq!(lines.next(), Some(".youtube.com\tTRUE\t/\tTRUE\t87400\tHSID\th"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn malformed_pairs_are_left_out() {
        let text = netscape("novalue; =anon; ok=1; bad=a\tb; ; evil=x\ny", 0);
        let cookies: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(cookies, vec![".youtube.com\tTRUE\t/\tTRUE\t86400\tok\t1"]);
    }

    #[test]
    fn the_file_is_written_and_deleted_with_its_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tmp");
        let neighbour = tmp.path().join("keep.txt");
        std::fs::write(&neighbour, b"not ours").unwrap();

        let guard = CookieFile::write(&dir, "SID=1; HSID=2", 0).unwrap().unwrap();
        let path = guard.path().to_path_buf();
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(is_ours(path.file_name().unwrap().to_str().unwrap()));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(body.contains("\tSID\t1\n") && body.contains("\tHSID\t2\n"));

        drop(guard);
        assert!(!path.exists(), "the guard deletes its file");
        assert!(dir.is_dir(), "never the folder");
        assert!(neighbour.exists(), "nor anything beside it");
    }

    #[test]
    fn two_runs_never_share_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = CookieFile::write(tmp.path(), "SID=1", 0).unwrap().unwrap();
        let b = CookieFile::write(tmp.path(), "SID=1", 0).unwrap().unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn a_header_with_no_cookie_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(CookieFile::write(tmp.path(), " ; junk", 0).unwrap().is_none());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_sweep_removes_only_our_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let stale = dir.join("cookies-0123456789abcdef.txt");
        std::fs::write(&stale, b"x").unwrap();
        let keep = [
            "cookies.txt",
            // Not the stale name in capitals: on a case-insensitive disk that is the same file.
            "cookies-89ABCDEF01234567.txt",
            "cookies-0123.txt",
            "cookies-0123456789abcdef.txt.bak",
            "other-0123456789abcdef.txt",
        ];
        for name in keep {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // A folder with our name pattern is not a file this module wrote.
        std::fs::create_dir(dir.join("cookies-fedcba9876543210.txt")).unwrap();

        assert_eq!(sweep_stale(dir), 1);
        assert!(!stale.exists());
        for name in keep {
            assert!(dir.join(name).exists(), "{name}");
        }
        assert!(dir.join("cookies-fedcba9876543210.txt").is_dir());
        assert_eq!(sweep_stale(&dir.join("absent")), 0);
    }
}
