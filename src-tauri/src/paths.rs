//! Where LiMusic Forge keeps its data, decided once: the one place allowed to ask Tauri for an app
//! data directory.
//!
//! **Portable mode.** A `data` directory next to the executable (next to the `.AppImage` on Linux)
//! turns it on, the same convention as VS Code's portable build. Everything then lives under it:
//! the SQLite file, the log, the caches, the window geometry and the webview profile. Nothing is
//! written to `%APPDATA%`/`%LOCALAPPDATA%` (or `~/.local/share`), so the folder can be carried on a
//! stick and deleting it removes every trace.
//!
//! `data` has to be a directory and writable; a read-only one (an unzip into Program Files) falls
//! back to the installed layout, and why is logged once logging is up (`portable_problem`). macOS
//! is never portable: a bundle's own directory is not a place to write.
//!
//! The single instance lock is still per identifier, so a portable copy and an installed one cannot
//! run at the same time. Accepted.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::{AppHandle, Manager};

/// The outcome of looking for `data` next to the executable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Detect {
    /// The directory holding `data`, when portable mode is on.
    pub root: Option<PathBuf>,
    /// Why a `data` that was there could not be used. Logged, not shown.
    pub problem: Option<String>,
}

static DETECTED: OnceLock<Detect> = OnceLock::new();

fn detected() -> &'static Detect {
    DETECTED.get_or_init(|| {
        #[cfg(any(windows, target_os = "linux"))]
        {
            let Ok(exe) = std::env::current_exe() else { return Detect::default() };
            let appimage = std::env::var_os("APPIMAGE");
            match base_dir(&exe, appimage.as_deref().map(Path::new)) {
                Some(dir) => detect(&dir, probe_write),
                None => Detect::default(),
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Detect::default()
        }
    })
}

/// The directory `data` is looked for in: the AppImage's own directory when running as one (the
/// executable is then inside a read-only mount under /tmp), the executable's otherwise.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn base_dir(exe: &Path, appimage: Option<&Path>) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(img) = appimage.filter(|p| !p.as_os_str().is_empty()) {
        return img.parent().map(Path::to_path_buf);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = appimage;
    exe.parent().map(Path::to_path_buf)
}

/// Portable when `<exe_dir>/data` is a directory that `probe` can write to. Pure apart from the
/// `is_dir`/`exists` checks, so the decision is testable with a temp dir and an injected probe.
pub(crate) fn detect(exe_dir: &Path, probe: impl Fn(&Path) -> io::Result<()>) -> Detect {
    let data = exe_dir.join("data");
    if !data.exists() {
        return Detect::default();
    }
    if !data.is_dir() {
        return Detect {
            root: None,
            problem: Some(format!(
                "{} exists but is not a directory; running installed",
                data.display()
            )),
        };
    }
    match probe(&data) {
        Ok(()) => Detect { root: Some(exe_dir.to_path_buf()), problem: None },
        Err(e) => Detect {
            root: None,
            problem: Some(format!("{} is not writable ({e}); running installed", data.display())),
        },
    }
}

/// Create and delete `data/.probe-<pid>`: the only reliable answer to "can we write here" (ACLs and
/// read-only media both fool a metadata check).
fn probe_write(data: &Path) -> io::Result<()> {
    let file = data.join(format!(".probe-{}", std::process::id()));
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&file)?;
    std::fs::remove_file(&file)
}

/// The directory holding `data`, when running portable.
pub fn portable_root() -> Option<&'static Path> {
    detected().root.as_deref()
}

pub fn is_portable() -> bool {
    portable_root().is_some()
}

/// Why portable mode was not used although a `data` entry was there. Log it after `init_logging`.
pub fn portable_problem() -> Option<&'static str> {
    detected().problem.as_deref()
}

/// Where the SQLite file, the log, the caches and the custom icon live.
pub fn data_dir(app: &AppHandle) -> PathBuf {
    if let Some(root) = portable_root() {
        return root.join("data");
    }
    match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            let tmp = std::env::temp_dir();
            tracing::warn!(error = %e, dir = %tmp.display(), "no app data dir, using the temp dir");
            tmp
        }
    }
}

/// The webview profile (localStorage, the login session's cookies) in portable mode. `None` when
/// installed, where Tauri's default stays in charge.
pub fn webview_dir() -> Option<PathBuf> {
    portable_root().map(|r| r.join("data").join("webview"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(_: &Path) -> io::Result<()> {
        Ok(())
    }

    #[test]
    fn paths_no_data_dir_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect(tmp.path(), ok), Detect::default());
    }

    #[test]
    fn paths_writable_data_dir_is_portable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        let d = detect(tmp.path(), probe_write);
        assert_eq!(d.root.as_deref(), Some(tmp.path()));
        assert_eq!(d.problem, None);
        // The probe cleans up after itself.
        assert_eq!(std::fs::read_dir(tmp.path().join("data")).unwrap().count(), 0);
    }

    #[test]
    fn paths_data_file_is_installed_with_a_problem() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data"), b"").unwrap();
        let d = detect(tmp.path(), ok);
        assert_eq!(d.root, None);
        assert!(d.problem.unwrap().contains("not a directory"));
    }

    #[test]
    fn paths_unwritable_data_dir_is_installed_with_a_problem() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        let d = detect(tmp.path(), |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "ro")));
        assert_eq!(d.root, None);
        assert!(d.problem.unwrap().contains("not writable"));
    }

    #[test]
    fn paths_base_dir_is_the_exe_dir() {
        let exe = Path::new("/opt/forge/limusic-forge");
        assert_eq!(base_dir(exe, None), Some(PathBuf::from("/opt/forge")));
    }

    /// Linux: `data` goes next to the `.AppImage`, not next to the binary in its /tmp mount.
    #[cfg(target_os = "linux")]
    #[test]
    fn paths_appimage_dir_wins_on_linux() {
        let exe = Path::new("/tmp/.mount_abc/usr/bin/limusic-forge");
        let img = Path::new("/home/u/Apps/LiMusic.AppImage");
        assert_eq!(base_dir(exe, Some(img)), Some(PathBuf::from("/home/u/Apps")));
        // An empty APPIMAGE is no AppImage.
        assert_eq!(
            base_dir(exe, Some(Path::new(""))),
            Some(PathBuf::from("/tmp/.mount_abc/usr/bin"))
        );
    }

    /// Off Linux `$APPIMAGE` means nothing, whatever it holds.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn paths_appimage_ignored_off_linux() {
        let exe = Path::new("C:/Forge/limusic-forge.exe");
        let img = Path::new("C:/Other/LiMusic.AppImage");
        assert_eq!(base_dir(exe, Some(img)), Some(PathBuf::from("C:/Forge")));
    }

    /// Every data path goes through [`data_dir`], or portable mode leaks files into the user's
    /// profile. The needles are assembled with `concat!` so this file's own text never matches.
    #[test]
    fn no_app_data_dir_outside_paths() {
        let needles = [
            concat!(".app_", "data_dir()"),
            concat!(".app_", "local_data_dir()"),
            concat!(".app_", "config_dir()"),
        ];
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src.clone()];
        let mut hits = Vec::new();
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") && p != src.join("paths.rs") {
                    seen += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for (i, line) in text.lines().enumerate() {
                        if needles.iter().any(|n| line.contains(n)) {
                            hits.push(format!("{}:{}", p.display(), i + 1));
                        }
                    }
                }
            }
        }
        assert!(seen > 10, "walked too few files ({seen}); wrong directory?");
        assert!(hits.is_empty(), "use paths::data_dir instead:\n{}", hits.join("\n"));
    }
}
