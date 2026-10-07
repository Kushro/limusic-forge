//! PlaylistForge's scheduled task (D35). PlaylistForge registered a daily "PlaylistForge Monitor"
//! task (pf-app `windows_task.rs`); once Forge has taken over, the import offers to remove it, and
//! only after the user confirms in the UI. Only that task, by its exact name:
//! `schtasks /delete /tn "PlaylistForge Monitor" /f`, run with `CREATE_NO_WINDOW`.

/// The task's exact name.
pub const PF_TASK_NAME: &str = "PlaylistForge Monitor";

/// `schtasks /delete /tn "PlaylistForge Monitor" /f`, as arguments (no shell: the name with its
/// space is one argument).
pub fn delete_args() -> [&'static str; 4] {
    ["/delete", "/tn", PF_TASK_NAME, "/f"]
}

/// `schtasks /query /tn "PlaylistForge Monitor"`: exit code 0 when it exists, whatever language
/// Windows answers in.
pub fn query_args() -> [&'static str; 3] {
    ["/query", "/tn", PF_TASK_NAME]
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<(i32, String), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // The system's own copy, not whatever `schtasks` comes first on PATH.
    let exe = std::env::var_os("SystemRoot")
        .map(|root| std::path::Path::new(&root).join("System32").join("schtasks.exe"))
        .filter(|p| p.is_file())
        .ok_or("schtasks_missing")?;
    let out = std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;
    Ok((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).trim().to_owned()))
}

/// Whether the task is registered. `None` off Windows, or when the Task Scheduler cannot be asked.
#[cfg(windows)]
pub fn exists() -> Option<bool> {
    schtasks(&query_args()).ok().map(|(code, _)| code == 0)
}

#[cfg(not(windows))]
pub fn exists() -> Option<bool> {
    None
}

/// Delete the task. Call only after the user confirmed it.
#[cfg(windows)]
pub fn unregister() -> Result<(), String> {
    let (code, stderr) = schtasks(&delete_args())?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("schtasks_failed: {stderr}"))
    }
}

#[cfg(not(windows))]
pub fn unregister() -> Result<(), String> {
    Err("unsupported".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pf_task_delete_is_exactly_the_one_task() {
        assert_eq!(delete_args(), ["/delete", "/tn", "PlaylistForge Monitor", "/f"]);
        assert_eq!(query_args(), ["/query", "/tn", "PlaylistForge Monitor"]);
        // Not Forge's own task, and no wildcard or folder.
        assert_ne!(PF_TASK_NAME, crate::wintask::TASK_NAME);
        assert!(!PF_TASK_NAME.contains(['*', '\\', '/']));
    }
}
