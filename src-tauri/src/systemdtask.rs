//! The Linux twin of wintask.rs: a systemd USER timer that runs the playlist check once a day,
//! app open or not: `"<exe>" --monitor --all` (headless.rs). No root, no cron.
//!
//! - The unit files are pure functions ([`service_unit`], [`timer_unit`]), so the quoting of an exe
//!   path with spaces or systemd's specifiers (`%` is `%%`) is tested without a systemd. Only the
//!   thin `systemctl` calls below are side-effecting, and the tests never reach them.
//! - Only [`SERVICE_FILE`] and [`TIMER_FILE`] are ever written, enabled, queried or removed. Other
//!   units in the same directory are none of our business.
//! - `Persistent=true`: on a dual boot the machine is often off (or in Windows) at the chosen time;
//!   the check then runs at the next Linux boot instead of being skipped.
//! - Without a systemd user manager (another init, a container) nothing is registered and the
//!   status says `no_systemd_user`: the UI explains it. There is no cron fallback (Q9).
//! - `status` reads values by property name (`systemctl --user show -p ...`), never by labels,
//!   and the registered exe from the `.service` we wrote.
//! - Under an AppImage `current_exe()` is the `/tmp/.mount_*` binary, which vanishes on exit:
//!   [`target_exe`] registers `$APPIMAGE` instead. If that file moves, `exe_moved` says so.
//! - The unattended run inherits the user's session bus when a graphical session exists. Without
//!   it the keyring may be unavailable; the monitor already reports that per account.

// Off Linux only the tests use the pure half.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Our two units. Never anything else.
pub const SERVICE_FILE: &str = "limusic-forge-monitor.service";
pub const TIMER_FILE: &str = "limusic-forge-monitor.timer";
/// The status/register error when there is no systemd user manager to talk to.
pub const NO_SYSTEMD_USER: &str = "no_systemd_user";

/// A path or argument as one quoted systemd word: double quotes, `\` and `"` escaped, `%` doubled
/// (specifiers are expanded inside quotes too), and a newline as `\n`.
fn quote(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `limusic-forge-monitor.service`: a oneshot that runs `"<exe>" --monitor --all`.
pub fn service_unit(exe: &Path) -> String {
    format!(
        "[Unit]\nDescription=LiMusic Forge Monitor\n\n[Service]\nType=oneshot\nExecStart={} --monitor --all\n",
        quote(&exe.to_string_lossy())
    )
}

/// `limusic-forge-monitor.timer`: every day at `hh_mm` (`HH:MM`, already normalized), catching up
/// at the next boot when the machine was off.
pub fn timer_unit(hh_mm: &str) -> String {
    format!(
        "[Unit]\nDescription=LiMusic Forge Monitor (daily)\n\n[Timer]\nOnCalendar=*-*-* {hh_mm}:00\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
    )
}

/// Where user units live: `$XDG_CONFIG_HOME/systemd/user`, else `~/.config/systemd/user`. A
/// relative or empty `XDG_CONFIG_HOME` is ignored, as the XDG spec says.
pub fn unit_dir(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|p| p.is_absolute()) {
        Some(x) => x.to_path_buf(),
        None => home.filter(|p| !p.as_os_str().is_empty())?.join(".config"),
    };
    Some(base.join("systemd").join("user"))
}

/// The exe the unit should run: `$APPIMAGE` when set and non-empty (never the `/tmp/.mount_*`
/// binary that disappears when the app closes), otherwise the running exe.
pub fn target_exe(current_exe: &Path, appimage_env: Option<&OsStr>) -> PathBuf {
    match appimage_env.filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => current_exe.to_path_buf(),
    }
}

/// What `systemctl --user show` said about the timer. `None` for an empty value or `n/a`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShowInfo {
    pub next_run: Option<String>,
    pub last_run: Option<String>,
    pub active_state: Option<String>,
    pub unit_file_state: Option<String>,
}

/// Arguments of the `show` call; its output is read by [`parse_show`].
pub fn show_args() -> Vec<String> {
    [
        "--user",
        "show",
        TIMER_FILE,
        "-p",
        "NextElapseUSecRealtime",
        "-p",
        "LastTriggerUSec",
        "-p",
        "ActiveState",
        "-p",
        "UnitFileState",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `KEY=VALUE` lines, by key (order and extra lines do not matter).
pub fn parse_show(text: &str) -> ShowInfo {
    let mut info = ShowInfo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let value = (!value.is_empty() && value != "n/a").then(|| value.to_owned());
        match key.trim() {
            "NextElapseUSecRealtime" => info.next_run = value,
            "LastTriggerUSec" => info.last_run = value,
            "ActiveState" => info.active_state = value,
            "UnitFileState" => info.unit_file_state = value,
            _ => {}
        }
    }
    info
}

/// Whether the timer counts as registered: its file is there and nobody disabled or masked it.
pub fn is_registered(timer_file_exists: bool, unit_file_state: Option<&str>) -> bool {
    timer_file_exists && !matches!(unit_file_state, Some("disabled" | "masked" | "masked-runtime"))
}

/// The exe in our `.service`: the first word of `ExecStart=`, undoing [`quote`].
pub fn registered_exe(service_text: &str) -> Option<String> {
    let line = service_text.lines().find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let line = line.trim();
    let mut out = String::new();
    if let Some(rest) = line.strip_prefix('"') {
        let mut chars = rest.chars().peekable();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    other => out.push(other),
                },
                '%' if chars.peek() == Some(&'%') => {
                    chars.next();
                    out.push('%');
                }
                c => out.push(c),
            }
        }
    } else {
        out.push_str(line.split_whitespace().next()?);
    }
    (!out.is_empty()).then_some(out)
}

/// Write `contents` to `dir/name` through a temp file in the same directory and a rename, so a
/// reader never sees half a unit.
fn write_atomic(dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, dir.join(name)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Write both unit files into `dir` (created if missing).
pub fn write_units(dir: &Path, exe: &Path, hh_mm: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(dir, SERVICE_FILE, &service_unit(exe))?;
    write_atomic(dir, TIMER_FILE, &timer_unit(hh_mm))
}

/// Remove our two files from `dir`; a missing one is fine.
pub fn remove_units(dir: &Path) -> std::io::Result<()> {
    for name in [TIMER_FILE, SERVICE_FILE] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// --- the systemctl side (Linux only; never called from the tests) ---

/// Run `systemctl` with `args` (no shell). Answers (exit code, stdout, stderr).
#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<(i32, String, String), String> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    ))
}

/// `systemctl --user` with `args`, an error when it exits non-zero.
#[cfg(target_os = "linux")]
fn systemctl_user_ok(args: &[&str]) -> Result<String, String> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    let (code, stdout, stderr) = systemctl(&full)?;
    if code != 0 {
        let why = if stderr.is_empty() { stdout.trim().to_owned() } else { stderr };
        return Err(format!("systemctl --user {} failed ({code}): {why}", args.join(" ")));
    }
    Ok(stdout)
}

/// Whether a systemd user manager answers (no `systemctl`, another init or a container: no).
#[cfg(target_os = "linux")]
fn user_manager_available() -> bool {
    matches!(systemctl(&["--user", "show-environment"]), Ok((0, _, _)))
}

#[cfg(target_os = "linux")]
fn current_unit_dir() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    unit_dir(xdg.as_deref(), home.as_deref())
}

/// Write the units and start the timer: `exe --monitor --all` daily at `hh_mm`. Registering again
/// replaces the files and restarts the timer, so a new time or a moved AppImage just works.
#[cfg(target_os = "linux")]
pub fn register(exe: &Path, hh_mm: &str) -> Result<(), String> {
    let hh_mm = crate::wintask::normalize_time(hh_mm).ok_or("bad_time")?;
    if !user_manager_available() {
        return Err(NO_SYSTEMD_USER.into());
    }
    let dir = current_unit_dir().ok_or("no home directory")?;
    write_units(&dir, exe, &hh_mm).map_err(|e| format!("writing the systemd units: {e}"))?;
    systemctl_user_ok(&["daemon-reload"])?;
    systemctl_user_ok(&["enable", "--now", TIMER_FILE])?;
    // An already running timer keeps its old schedule until restarted.
    systemctl_user_ok(&["restart", TIMER_FILE])?;
    tracing::info!(time = %hh_mm, exe = %exe.display(), "registered the systemd user timer");
    Ok(())
}

/// Stop and remove the timer and its service. Not being registered is no error.
#[cfg(target_os = "linux")]
pub fn unregister() -> Result<(), String> {
    let Some(dir) = current_unit_dir() else { return Ok(()) };
    if !dir.join(TIMER_FILE).exists() && !dir.join(SERVICE_FILE).exists() {
        return Ok(());
    }
    let available = user_manager_available();
    if available && dir.join(TIMER_FILE).exists() {
        if let Err(e) = systemctl_user_ok(&["disable", "--now", TIMER_FILE]) {
            tracing::warn!(error = %e, "could not disable the systemd user timer");
        }
    }
    remove_units(&dir).map_err(|e| format!("removing the systemd units: {e}"))?;
    if available {
        systemctl_user_ok(&["daemon-reload"])?;
    }
    tracing::info!("removed the systemd user timer");
    Ok(())
}

/// What Settings shows about the timer.
#[cfg(target_os = "linux")]
pub fn status(schedule_time: String) -> crate::wintask::WinTaskStatus {
    use crate::wintask::{exe_moved, WinTaskStatus};
    if !user_manager_available() {
        return WinTaskStatus {
            supported: false,
            schedule_time,
            error: Some(NO_SYSTEMD_USER.into()),
            ..WinTaskStatus::default()
        };
    }
    let current = std::env::current_exe()
        .ok()
        .map(|c| target_exe(&c, std::env::var_os("APPIMAGE").as_deref()));
    let mut out = WinTaskStatus {
        supported: true,
        schedule_time,
        current_exe: current.as_ref().map(|p| p.to_string_lossy().into_owned()),
        ..WinTaskStatus::default()
    };
    let Some(dir) = current_unit_dir() else {
        out.error = Some("no home directory".into());
        return out;
    };
    let file_exists = dir.join(TIMER_FILE).is_file();
    let show = if file_exists {
        match systemctl(&show_args().iter().map(String::as_str).collect::<Vec<_>>()) {
            Ok((0, stdout, _)) => parse_show(&stdout),
            Ok((code, _, stderr)) => {
                out.error = Some(format!("systemctl show failed ({code}): {stderr}"));
                ShowInfo::default()
            }
            Err(e) => {
                out.error = Some(e);
                ShowInfo::default()
            }
        }
    } else {
        ShowInfo::default()
    };
    out.registered = is_registered(file_exists, show.unit_file_state.as_deref());
    if out.registered {
        out.registered_exe =
            std::fs::read_to_string(dir.join(SERVICE_FILE)).ok().and_then(|t| registered_exe(&t));
        out.exe_moved =
            current.as_deref().is_some_and(|c| exe_moved(out.registered_exe.as_deref(), c));
        out.next_run = show.next_run;
        out.last_run = show.last_run;
        out.status = show.active_state;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACED: &str = "/home/luis/Apps/LiMusic Forge/limusic-forge.AppImage";

    #[test]
    fn systemdtask_unit_names_are_ours() {
        assert_eq!(SERVICE_FILE, "limusic-forge-monitor.service");
        assert_eq!(TIMER_FILE, "limusic-forge-monitor.timer");
        assert!(show_args().contains(&TIMER_FILE.to_string()));
    }

    #[test]
    fn systemdtask_service_quotes_a_path_with_spaces() {
        let s = service_unit(Path::new(SPACED));
        assert_eq!(
            s,
            "[Unit]\nDescription=LiMusic Forge Monitor\n\n[Service]\nType=oneshot\nExecStart=\"/home/luis/Apps/LiMusic Forge/limusic-forge.AppImage\" --monitor --all\n"
        );
    }

    #[test]
    fn systemdtask_service_escapes_systemd_specials() {
        let s = service_unit(Path::new("/opt/100%/od\"d\\dir/forge"));
        assert!(
            s.contains("ExecStart=\"/opt/100%%/od\\\"d\\\\dir/forge\" --monitor --all\n"),
            "{s}"
        );
    }

    #[test]
    fn systemdtask_timer_is_daily_persistent_and_installable() {
        let t = timer_unit("07:30");
        assert!(t.contains("OnCalendar=*-*-* 07:30:00\n"));
        assert!(t.contains("Persistent=true\n"));
        assert!(t.contains("[Install]\nWantedBy=timers.target\n"));
        assert!(t.contains("[Timer]"));
    }

    /// Unix only: `/custom/cfg` is not an absolute path on Windows.
    #[cfg(unix)]
    #[test]
    fn systemdtask_unit_dir_prefers_xdg_then_home() {
        let xdg = Path::new("/custom/cfg");
        let home = Path::new("/home/luis");
        assert_eq!(
            unit_dir(Some(xdg), Some(home)),
            Some(PathBuf::from("/custom/cfg/systemd/user"))
        );
        assert_eq!(
            unit_dir(None, Some(home)),
            Some(PathBuf::from("/home/luis/.config/systemd/user"))
        );
        // Relative or empty XDG_CONFIG_HOME is ignored.
        assert_eq!(
            unit_dir(Some(Path::new("rel")), Some(home)),
            Some(PathBuf::from("/home/luis/.config/systemd/user"))
        );
        assert_eq!(
            unit_dir(Some(Path::new("")), Some(home)),
            Some(PathBuf::from("/home/luis/.config/systemd/user"))
        );
        assert_eq!(unit_dir(None, None), None);
    }

    #[test]
    fn systemdtask_target_exe_prefers_appimage() {
        let mounted = Path::new("/tmp/.mount_LiMusiabc123/usr/bin/limusic-forge");
        assert_eq!(
            target_exe(mounted, Some(OsStr::new("/home/luis/LiMusic.AppImage"))),
            PathBuf::from("/home/luis/LiMusic.AppImage")
        );
        assert_eq!(target_exe(mounted, Some(OsStr::new(""))), mounted);
        assert_eq!(target_exe(mounted, None), mounted);
    }

    #[test]
    fn systemdtask_show_is_read_by_name() {
        let text = "ActiveState=active\nNextElapseUSecRealtime=Thu 2026-10-29 09:00:00 CEST\n\
                    LastTriggerUSec=n/a\nUnitFileState=enabled\nSomethingElse=1\nnoequals\n";
        let info = parse_show(text);
        assert_eq!(info.next_run.as_deref(), Some("Thu 2026-10-29 09:00:00 CEST"));
        assert_eq!(info.last_run, None);
        assert_eq!(info.active_state.as_deref(), Some("active"));
        assert_eq!(info.unit_file_state.as_deref(), Some("enabled"));
        assert_eq!(parse_show(""), ShowInfo::default());
        let empty = parse_show("NextElapseUSecRealtime=\nActiveState=inactive\n");
        assert_eq!(empty.next_run, None);
        assert_eq!(empty.active_state.as_deref(), Some("inactive"));
    }

    #[test]
    fn systemdtask_registered_needs_the_file_and_not_disabled() {
        assert!(is_registered(true, Some("enabled")));
        assert!(is_registered(true, None));
        assert!(!is_registered(true, Some("disabled")));
        assert!(!is_registered(true, Some("masked")));
        assert!(!is_registered(false, Some("enabled")));
    }

    #[test]
    fn systemdtask_exe_round_trips_through_the_service() {
        for exe in [SPACED, "/opt/100%/od\"d\\dir/forge", "/usr/bin/limusic-forge"] {
            let text = service_unit(Path::new(exe));
            assert_eq!(registered_exe(&text).as_deref(), Some(exe), "{exe}");
        }
        assert_eq!(
            registered_exe("[Service]\nExecStart=/usr/bin/forge --monitor --all\n").as_deref(),
            Some("/usr/bin/forge")
        );
        assert_eq!(registered_exe("[Service]\nType=oneshot\n"), None);
        assert_eq!(registered_exe("ExecStart=\"unterminated"), None);
        assert_eq!(registered_exe("ExecStart=\"\" --monitor"), None);
    }

    #[test]
    fn systemdtask_writes_and_removes_only_our_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("systemd").join("user");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("other.service"), "keep me").unwrap();

        write_units(&dir, Path::new(SPACED), "06:05").unwrap();
        let service = std::fs::read_to_string(dir.join(SERVICE_FILE)).unwrap();
        assert_eq!(registered_exe(&service).as_deref(), Some(SPACED));
        let timer = std::fs::read_to_string(dir.join(TIMER_FILE)).unwrap();
        assert!(timer.contains("OnCalendar=*-*-* 06:05:00"));
        // Writing again replaces, leaving no temp files behind.
        write_units(&dir, Path::new("/new/forge"), "07:00").unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["limusic-forge-monitor.service", "limusic-forge-monitor.timer", "other.service"]
        );

        remove_units(&dir).unwrap();
        remove_units(&dir).unwrap(); // already gone: no error
        assert!(!dir.join(SERVICE_FILE).exists() && !dir.join(TIMER_FILE).exists());
        assert_eq!(std::fs::read_to_string(dir.join("other.service")).unwrap(), "keep me");
    }
}
