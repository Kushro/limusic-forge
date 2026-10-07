//! The Windows scheduled task that runs the playlist check once a day, app open or not:
//! `"<exe>" --monitor --all` (headless.rs). Port of PlaylistForge's `monitor/windows_task.rs`.
//!
//! - The argument vectors for `schtasks` are pure functions ([`create_args`], [`delete_args`],
//!   [`query_args`]), so the quoting of an exe path with spaces is tested without touching the real
//!   Task Scheduler.
//! - The query reads `/v /fo CSV /nh`: fields by position, never by their (translated) labels,
//!   and whether the task exists comes from the exit code alone. PlaylistForge parsed the English
//!   `/fo LIST` labels, which a Spanish Windows does not print.
//! - Only [`TASK_NAME`] is ever created, queried or deleted here. PlaylistForge's own task
//!   ("PlaylistForge Monitor") is the importer's business, with the user's confirmation (D35).
//! - `schtasks` runs with `CREATE_NO_WINDOW`, so no console flashes up.

// Off Windows only the stubs and the tests use the pure half.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

use crate::db::Db;

/// The task's name in the Task Scheduler. Never PlaylistForge's.
pub const TASK_NAME: &str = "LiMusic Forge Monitor";
/// When the task runs, `HH:MM` local time, as last registered from Settings.
pub const SCHEDULE_TIME_KEY: &str = "monitor.schedule_time";
pub const DEFAULT_SCHEDULE_TIME: &str = "09:00";

/// The `/tr` value: the command line the task runs. The exe is always quoted (paths routinely
/// hold spaces), and a quote inside it is escaped so the value stays one quoted string.
pub fn tr_value(exe: &Path) -> String {
    let raw = exe.to_string_lossy();
    format!("\"{}\" --monitor --all", raw.replace('"', "\\\""))
}

/// `schtasks /create`: daily at `hh_mm`. `/f` replaces an existing task of the same name, so
/// registering again (a new time, a moved portable copy) just works.
pub fn create_args(exe: &Path, hh_mm: &str) -> Vec<String> {
    let tr = tr_value(exe);
    ["/create", "/tn", TASK_NAME, "/tr", tr.as_str(), "/sc", "daily", "/st", hh_mm, "/f"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `schtasks /delete`, without its confirmation prompt.
pub fn delete_args() -> Vec<String> {
    ["/delete", "/tn", TASK_NAME, "/f"].iter().map(|s| s.to_string()).collect()
}

/// `schtasks /query`, verbose CSV without the header: one row of fields in a fixed order whatever
/// the language (see [`parse_query`]). Verbose because only it carries the command the task runs.
pub fn query_args() -> Vec<String> {
    ["/query", "/tn", TASK_NAME, "/v", "/fo", "CSV", "/nh"].iter().map(|s| s.to_string()).collect()
}

/// `H:MM` or `HH:MM` (24 h) as `HH:MM`; `None` for anything else.
pub fn normalize_time(raw: &str) -> Option<String> {
    let (h, m) = raw.trim().split_once(':')?;
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(h, 1..=2) || !digits(m, 2..=2) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then(|| format!("{h:02}:{m:02}"))
}

/// The time the task runs at, from the setting; the default when unset or unreadable.
pub fn schedule_time(db: &Db) -> String {
    db.get_setting(SCHEDULE_TIME_KEY)
        .and_then(|t| normalize_time(&t))
        .unwrap_or_else(|| DEFAULT_SCHEDULE_TIME.to_owned())
}

/// CSV as `schtasks` writes it: comma-separated, fields in double quotes, a quote inside a field
/// doubled. Quoted fields may span lines; blank lines are skipped.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(std::mem::take(&mut row));
                }
                row.clear();
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// What the query says about the task. Values are as `schtasks` printed them (in the system's
/// language and date format); `None` for an empty field.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TaskInfo {
    pub next_run: Option<String>,
    pub status: Option<String>,
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    /// The command line the task runs (`Task To Run`).
    pub task_to_run: Option<String>,
}

/// The first row of the query. Verbose rows are `HostName, TaskName, Next Run Time, Status,
/// Logon Mode, Last Run Time, Last Result, Author, Task To Run, ...`; a plain row (no `/v`) is
/// `TaskName, Next Run Time, Status`. `None` when there is no row to read.
pub fn parse_query(text: &str) -> Option<TaskInfo> {
    let row = parse_csv(text).into_iter().find(|r| r.len() >= 3)?;
    let at = |i: usize| row.get(i).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    Some(if row.len() >= 9 {
        TaskInfo {
            next_run: at(2),
            status: at(3),
            last_run: at(5),
            last_result: at(6),
            task_to_run: at(8),
        }
    } else {
        TaskInfo { next_run: at(1), status: at(2), ..TaskInfo::default() }
    })
}

/// The executable in a task's command line: the quoted part when it starts with a quote. Otherwise
/// up to the first `.exe` that ends a word, since `schtasks /v` prints "Task To Run" without the
/// quotes the task was registered with (`E:\Tools\LiMusic Forge\limusic-forge.exe --monitor`);
/// the first word when there is no `.exe` at all.
pub fn exe_of(command: &str) -> Option<String> {
    let command = command.trim();
    let exe = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => {
            let lower = command.to_ascii_lowercase();
            let end = lower
                .match_indices(".exe")
                .map(|(i, _)| i + 4)
                .find(|&end| command[end..].chars().next().is_none_or(char::is_whitespace));
            match end {
                Some(end) => &command[..end],
                None => command.split_whitespace().next()?,
            }
        }
    };
    (!exe.is_empty()).then(|| exe.to_owned())
}

/// Whether the task runs a different exe than `current`: a portable copy that was moved or a
/// second copy that registered it. Paths compare as Windows does, ignoring case and slash style.
/// A path `schtasks` printed in another code page (`\u{FFFD}` after decoding) is not judged.
pub fn exe_moved(registered: Option<&str>, current: &Path) -> bool {
    let Some(registered) = registered.filter(|r| !r.contains('\u{FFFD}')) else { return false };
    let norm = |s: &str| {
        let s = s.trim().replace('/', "\\");
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase()
    };
    norm(registered) != norm(&current.to_string_lossy())
}

/// What Settings shows about the task.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct WinTaskStatus {
    /// `false` off Windows: there is no task to register.
    pub supported: bool,
    pub registered: bool,
    /// The time from the setting (`HH:MM`), what registering uses.
    pub schedule_time: String,
    pub next_run: Option<String>,
    pub status: Option<String>,
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    /// The exe the task runs, and this one's.
    pub registered_exe: Option<String>,
    pub current_exe: Option<String>,
    /// The task runs some other copy (see [`exe_moved`]).
    pub exe_moved: bool,
    /// Why the query itself could not run.
    pub error: Option<String>,
}

/// [`WinTaskStatus`] from a query's result (`Ok(None)`: no such task). Pure.
pub fn status_from(
    query: Result<Option<TaskInfo>, String>,
    schedule_time: String,
    current: Option<&Path>,
) -> WinTaskStatus {
    let mut out = WinTaskStatus {
        supported: true,
        schedule_time,
        current_exe: current.map(|p| p.to_string_lossy().into_owned()),
        ..WinTaskStatus::default()
    };
    match query {
        Ok(Some(info)) => {
            let registered_exe = info.task_to_run.as_deref().and_then(exe_of);
            out.registered = true;
            out.exe_moved = current.is_some_and(|c| exe_moved(registered_exe.as_deref(), c));
            out.registered_exe = registered_exe;
            out.next_run = info.next_run;
            out.status = info.status;
            out.last_run = info.last_run;
            out.last_result = info.last_result;
        }
        Ok(None) => {}
        Err(e) => out.error = Some(e),
    }
    out
}

/// Run `schtasks` with `args`, no console window. Answers (exit code, stdout, stderr).
#[cfg(windows)]
fn schtasks(args: &[String]) -> Result<(i32, String, String), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // The system's own copy, not whatever `schtasks` comes first on PATH.
    let exe = std::env::var_os("SystemRoot")
        .map(|root| Path::new(&root).join("System32").join("schtasks.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "schtasks.exe".into());
    let out = std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    ))
}

/// The task as the Task Scheduler has it; `Ok(None)` when it is not registered (the query's
/// exit code says so, in any language).
#[cfg(windows)]
pub fn query() -> Result<Option<TaskInfo>, String> {
    let (code, stdout, _) = schtasks(&query_args())?;
    if code != 0 {
        return Ok(None);
    }
    Ok(Some(parse_query(&stdout).unwrap_or_default()))
}

/// Create (or replace) the task: `exe --monitor --all`, daily at `hh_mm`.
#[cfg(windows)]
pub fn register(exe: &Path, hh_mm: &str) -> Result<(), String> {
    let hh_mm = normalize_time(hh_mm).ok_or("bad_time")?;
    let (code, stdout, stderr) = schtasks(&create_args(exe, &hh_mm))?;
    if code != 0 {
        let why = if stderr.is_empty() { stdout.trim().to_owned() } else { stderr };
        return Err(format!("schtasks /create failed ({code}): {why}"));
    }
    tracing::info!(time = %hh_mm, exe = %exe.display(), "registered the scheduled task");
    Ok(())
}

/// Delete the task. Not being registered is no error.
#[cfg(windows)]
pub fn unregister() -> Result<(), String> {
    if query()?.is_none() {
        return Ok(());
    }
    let (code, stdout, stderr) = schtasks(&delete_args())?;
    if code != 0 {
        let why = if stderr.is_empty() { stdout.trim().to_owned() } else { stderr };
        return Err(format!("schtasks /delete failed ({code}): {why}"));
    }
    tracing::info!("removed the scheduled task");
    Ok(())
}

#[cfg(windows)]
pub fn status(schedule_time: String) -> WinTaskStatus {
    let current = std::env::current_exe().ok();
    status_from(query(), schedule_time, current.as_deref())
}

#[cfg(not(windows))]
pub fn register(_exe: &Path, _hh_mm: &str) -> Result<(), String> {
    Err("unsupported".into())
}

#[cfg(not(windows))]
pub fn unregister() -> Result<(), String> {
    Err("unsupported".into())
}

#[cfg(not(windows))]
pub fn status(schedule_time: String) -> WinTaskStatus {
    WinTaskStatus { supported: false, schedule_time, ..WinTaskStatus::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SPACED: &str = r"C:\Program Files\LiMusic Forge\limusic-forge.exe";

    #[test]
    fn the_task_is_ours_and_never_playlistforges() {
        assert_eq!(TASK_NAME, "LiMusic Forge Monitor");
        let exe = PathBuf::from(SPACED);
        for args in [create_args(&exe, "09:00"), delete_args(), query_args()] {
            let i = args.iter().position(|a| a == "/tn").unwrap();
            assert_eq!(args[i + 1], TASK_NAME);
            assert!(args.iter().all(|a| !a.contains("PlaylistForge")));
        }
    }

    #[test]
    fn tr_quotes_a_path_with_spaces() {
        assert_eq!(
            tr_value(&PathBuf::from(SPACED)),
            r#""C:\Program Files\LiMusic Forge\limusic-forge.exe" --monitor --all"#
        );
    }

    #[test]
    fn tr_escapes_an_embedded_quote() {
        let exe = PathBuf::from(r#"C:\odd"dir\limusic-forge.exe"#);
        assert_eq!(tr_value(&exe), r#""C:\odd\"dir\limusic-forge.exe" --monitor --all"#);
    }

    #[test]
    fn create_args_shape() {
        let args = create_args(&PathBuf::from(SPACED), "07:30");
        assert_eq!(
            args,
            [
                "/create",
                "/tn",
                "LiMusic Forge Monitor",
                "/tr",
                r#""C:\Program Files\LiMusic Forge\limusic-forge.exe" --monitor --all"#,
                "/sc",
                "daily",
                "/st",
                "07:30",
                "/f",
            ]
        );
    }

    #[test]
    fn delete_and_query_args_shape() {
        assert_eq!(delete_args(), ["/delete", "/tn", "LiMusic Forge Monitor", "/f"]);
        assert_eq!(
            query_args(),
            ["/query", "/tn", "LiMusic Forge Monitor", "/v", "/fo", "CSV", "/nh"]
        );
    }

    #[test]
    fn times_are_normalized_or_refused() {
        assert_eq!(normalize_time("9:05").as_deref(), Some("09:05"));
        assert_eq!(normalize_time(" 23:59 ").as_deref(), Some("23:59"));
        assert_eq!(normalize_time("00:00").as_deref(), Some("00:00"));
        for bad in ["24:00", "12:60", "12:5", "123:00", "ab:cd", "1200", "", "12:00:00", "-1:00"] {
            assert_eq!(normalize_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn schedule_time_reads_the_setting_or_the_default() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        assert_eq!(schedule_time(&db), DEFAULT_SCHEDULE_TIME);
        db.set_setting(SCHEDULE_TIME_KEY, "6:15");
        assert_eq!(schedule_time(&db), "06:15");
        db.set_setting(SCHEDULE_TIME_KEY, "nonsense");
        assert_eq!(schedule_time(&db), DEFAULT_SCHEDULE_TIME);
    }

    #[test]
    fn csv_handles_quotes_commas_and_doubled_quotes() {
        let text = "\"a\",\"b, c\",\"\"\"q\"\" d\",plain\r\n\r\n\"x\",\"\"\r\n";
        assert_eq!(
            parse_csv(text),
            vec![vec!["a", "b, c", "\"q\" d", "plain"], vec!["x", ""]]
                .into_iter()
                .map(|r| r.into_iter().map(String::from).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        assert_eq!(parse_csv("\"no newline\""), vec![vec!["no newline".to_string()]]);
        assert!(parse_csv("").is_empty());
    }

    /// A verbose row as a Spanish Windows prints it: the labels would be translated, the order is
    /// not, and `/nh` drops the labels anyway.
    #[test]
    fn verbose_query_is_read_by_position_in_any_language() {
        let row = concat!(
            "\"PC\",\"\\LiMusic Forge Monitor\",\"16/07/2026 9:00:00\",\"Listo\",\"Interactivo\",",
            "\"15/07/2026 9:00:03\",\"0\",\"PC\\luis\",",
            "\"\"\"C:\\Program Files\\LiMusic Forge\\limusic-forge.exe\"\" --monitor --all\",",
            "\"N/D\",\"N/D\",\"Habilitado\"\r\n"
        );
        let info = parse_query(row).unwrap();
        assert_eq!(info.next_run.as_deref(), Some("16/07/2026 9:00:00"));
        assert_eq!(info.status.as_deref(), Some("Listo"));
        assert_eq!(info.last_run.as_deref(), Some("15/07/2026 9:00:03"));
        assert_eq!(info.last_result.as_deref(), Some("0"));
        assert_eq!(
            info.task_to_run.as_deref(),
            Some(r#""C:\Program Files\LiMusic Forge\limusic-forge.exe" --monitor --all"#)
        );
    }

    #[test]
    fn plain_query_row_and_nothing_to_read() {
        let info =
            parse_query("\"\\LiMusic Forge Monitor\",\"7/16/2026 9:00:00 AM\",\"Ready\"\r\n");
        let info = info.unwrap();
        assert_eq!(info.next_run.as_deref(), Some("7/16/2026 9:00:00 AM"));
        assert_eq!(info.status.as_deref(), Some("Ready"));
        assert_eq!(info.task_to_run, None);
        assert_eq!(parse_query(""), None);
        assert_eq!(parse_query("ERROR: The system cannot find the file specified.\r\n"), None);
    }

    #[test]
    fn exe_of_a_command_line() {
        assert_eq!(
            exe_of(r#""C:\Program Files\LiMusic Forge\limusic-forge.exe" --monitor --all"#)
                .as_deref(),
            Some(SPACED)
        );
        assert_eq!(
            exe_of(r"C:\Forge\limusic-forge.exe --monitor").as_deref(),
            Some(r"C:\Forge\limusic-forge.exe")
        );
        // What `schtasks /v` prints for a task registered with a quoted path: no quotes.
        assert_eq!(
            exe_of(r"E:\Tools\LiMusic Forge\limusic-forge.exe --monitor --all").as_deref(),
            Some(r"E:\Tools\LiMusic Forge\limusic-forge.exe")
        );
        assert_eq!(
            exe_of(r"D:\My.exes\LiMusic Forge\LIMUSIC-FORGE.EXE").as_deref(),
            Some(r"D:\My.exes\LiMusic Forge\LIMUSIC-FORGE.EXE")
        );
        assert_eq!(exe_of("   "), None);
        assert_eq!(exe_of("\"\" --monitor"), None);
    }

    #[test]
    fn a_moved_portable_copy_is_noticed() {
        let here = PathBuf::from(r"D:\Stick\LiMusic Forge\limusic-forge.exe");
        assert!(!exe_moved(Some(r"d:/stick/limusic forge/LIMUSIC-FORGE.EXE"), &here));
        assert!(!exe_moved(
            Some(r"D:\Stick\LiMusic Forge\limusic-forge.exe"),
            &PathBuf::from(r"\\?\D:\Stick\LiMusic Forge\limusic-forge.exe")
        ));
        assert!(exe_moved(Some(r"E:\Old\limusic-forge.exe"), &here));
        assert!(!exe_moved(None, &here));
        assert!(
            !exe_moved(Some("C:\\Usu\u{FFFD}rio\\limusic-forge.exe"), &here),
            "undecodable: not judged"
        );
    }

    #[test]
    fn status_from_a_query() {
        let here = PathBuf::from(r"D:\New\limusic-forge.exe");
        let info = TaskInfo {
            next_run: Some("tomorrow".into()),
            status: Some("Ready".into()),
            task_to_run: Some(r#""E:\Old\limusic-forge.exe" --monitor --all"#.into()),
            ..TaskInfo::default()
        };
        let s = status_from(Ok(Some(info)), "09:00".into(), Some(&here));
        assert!(s.supported && s.registered && s.exe_moved);
        assert_eq!(s.registered_exe.as_deref(), Some(r"E:\Old\limusic-forge.exe"));
        assert_eq!(s.next_run.as_deref(), Some("tomorrow"));

        let none = status_from(Ok(None), "09:00".into(), Some(&here));
        assert!(!none.registered && !none.exe_moved && none.error.is_none());
        let failed = status_from(Err("schtasks: not found".into()), "09:00".into(), None);
        assert!(!failed.registered);
        assert_eq!(failed.error.as_deref(), Some("schtasks: not found"));
    }
}
