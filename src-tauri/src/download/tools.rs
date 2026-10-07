//! The two external programs downloads run on: yt-dlp, and ffmpeg for its post-processing.
//!
//! On Windows the app manages them (`managed`): "Install / update" fetches them into
//! `<data>/bin` ([`bin_dir`], under `paths::data_dir`, so a portable copy keeps them in its own
//! folder). On Linux and macOS they come from the system, found on PATH, and nothing is
//! downloaded: a package manager keeps them current better than we could.
//!
//! Every download is checked against the checksums its own release publishes, from the same
//! release: yt-dlp's `SHA2-256SUMS` (stable `yt-dlp/yt-dlp`, or `yt-dlp/yt-dlp-nightly-builds`
//! for the nightly channel), and `checksums.sha256` of `yt-dlp/FFmpeg-Builds`. Both files and
//! the binary are taken from one `releases/latest` answer, so the sums always describe the bytes
//! that were fetched. The bytes go to a `.part` beside the target and are renamed into place only
//! once they match, so an interrupted or tampered download never leaves a runnable file; on a
//! mismatch that `.part`, and nothing else, is deleted (`checksum_mismatch`).
//!
//! Every child process is spawned with `CREATE_NO_WINDOW` ([`hidden_command`]): a windowed app
//! starting a console program otherwise flashes a console window.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// `CREATE_NO_WINDOW` from the Win32 process creation flags.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Whether this build installs and updates the tools itself (Windows) or uses the system's.
pub const MANAGED: bool = cfg!(windows);

#[cfg(windows)]
pub const YTDLP_EXE: &str = "yt-dlp.exe";
#[cfg(not(windows))]
pub const YTDLP_EXE: &str = "yt-dlp";
#[cfg(windows)]
pub const FFMPEG_EXE: &str = "ffmpeg.exe";
#[cfg(not(windows))]
pub const FFMPEG_EXE: &str = "ffmpeg";
/// Extracted beside ffmpeg: yt-dlp probes codecs with it when extracting audio.
pub const FFPROBE_EXE: &str = "ffprobe.exe";

/// yt-dlp's release asset for Windows x64, and the checksum file beside it.
pub const YTDLP_ASSET: &str = "yt-dlp.exe";
pub const YTDLP_SUMS_ASSET: &str = "SHA2-256SUMS";
/// The ffmpeg build yt-dlp's documentation points at, and its checksum file. The checksum
/// file's name is taken from the FFmpeg-Builds release page; it is not confirmed by a test that
/// reaches the network.
pub const FFMPEG_REPO: &str = "yt-dlp/FFmpeg-Builds";
pub const FFMPEG_ASSET: &str = "ffmpeg-master-latest-win64-gpl.zip";
pub const FFMPEG_SUMS_ASSET: &str = "checksums.sha256";

/// The zip `install_ffmpeg` downloads into `bin` and deletes afterwards. A fixed name, so the
/// cleanup deletes exactly the file this code wrote.
const FFMPEG_ZIP: &str = "ffmpeg-download.zip";

const USER_AGENT: &str = "LiMusic-Forge (downloads)";

/// `<data>/bin`.
pub fn bin_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bin")
}

/// The yt-dlp to run: the managed one in `bin` on Windows, the one on PATH elsewhere.
pub fn ytdlp_path(data_dir: &Path) -> Option<PathBuf> {
    if MANAGED {
        Some(bin_dir(data_dir).join(YTDLP_EXE)).filter(|p| p.is_file())
    } else {
        which(YTDLP_EXE)
    }
}

/// `--ffmpeg-location`: the managed `bin` when ffmpeg is in it. `None` on Linux and macOS,
/// where yt-dlp finds the system's on PATH by itself.
pub fn ffmpeg_dir(data_dir: &Path) -> Option<PathBuf> {
    let bin = bin_dir(data_dir);
    (MANAGED && bin.join(FFMPEG_EXE).is_file()).then_some(bin)
}

pub fn ffmpeg_present(data_dir: &Path) -> bool {
    if MANAGED {
        bin_dir(data_dir).join(FFMPEG_EXE).is_file()
    } else {
        which(FFMPEG_EXE).is_some()
    }
}

/// The first `name` on PATH that is a file.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// What Settings ▸ Downloads shows.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolsStatus {
    /// `Some` only when the binary is there and answered `--version`: a broken file is as
    /// unusable as a missing one.
    pub ytdlp_version: Option<String>,
    pub ffmpeg_present: bool,
    /// The app installs and updates them (Windows); otherwise they come from PATH.
    pub managed: bool,
}

pub async fn status(data_dir: &Path) -> ToolsStatus {
    ToolsStatus {
        ytdlp_version: ytdlp_version(data_dir).await,
        ffmpeg_present: ffmpeg_present(data_dir),
        managed: MANAGED,
    }
}

/// `yt-dlp --version`, trimmed; `None` when it is absent, does not start, fails or hangs.
pub async fn ytdlp_version(data_dir: &Path) -> Option<String> {
    let path = ytdlp_path(data_dir)?;
    let mut command = hidden_command(&path);
    command.arg("--version").stdin(std::process::Stdio::null());
    let output =
        tokio::time::timeout(Duration::from_secs(30), command.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// A `tokio::process::Command` that never flashes a console window, and whose child dies with
/// the handle (a dropped download task must not leave yt-dlp running). That reaches the direct
/// child only; [`ProcessTree`] covers what it starts.
pub fn hidden_command(program: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// A child together with everything it starts, so a cancel stops all of it.
///
/// Killing only the direct child is not enough: on Windows `yt-dlp.exe` is a PyInstaller
/// onefile (a bootloader that runs the real yt-dlp as its own child), and yt-dlp runs ffmpeg, so
/// the grandchildren would live on and keep writing. On Windows the child goes into a Job Object
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: [`ProcessTree::kill`] terminates the job, and
/// dropping the tree closes the job's handle, which kills whatever is still in it (a dropped run,
/// the app exiting). On Linux the child leads its own process group ([`ProcessTree::prepare`])
/// and a kill goes to the group, as does dropping the tree while the child is still there.
/// Elsewhere only the direct child is killed.
///
/// On Windows the child is put in the job just after it spawns, not created suspended and
/// resumed once inside: resuming needs its main thread's handle, which neither std nor tokio
/// hands back, and finding it (Toolhelp32) or `NtResumeProcess` needs `windows` features this
/// build does not have. What could slip out is a process the child starts within the
/// microseconds before [`ProcessTree::attach`], which runs right after `spawn` on the same thread;
/// yt-dlp's PyInstaller bootloader unpacks for far longer than that before starting anything.
#[derive(Default)]
pub struct ProcessTree {
    #[cfg(windows)]
    job: Option<job::Job>,
    #[cfg(target_os = "linux")]
    pgid: Option<i32>,
}

impl ProcessTree {
    /// What has to be set on the command before it spawns (Linux: a process group of its own).
    pub fn prepare(command: &mut tokio::process::Command) {
        #[cfg(target_os = "linux")]
        command.process_group(0);
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }

    /// Takes in the freshly spawned `child`. A tree that cannot be set up degrades to killing the
    /// direct child alone, and says so.
    #[cfg(windows)]
    pub fn attach(child: &tokio::process::Child) -> Self {
        let job = child.raw_handle().and_then(|handle| match job::Job::with_process(handle) {
            Ok(job) => Some(job),
            Err(e) => {
                tracing::warn!(error = %e, "downloads: no job object, killing yt-dlp alone");
                None
            }
        });
        Self { job }
    }

    /// Takes in the freshly spawned `child`, whose pid `process_group(0)` made its group id.
    #[cfg(target_os = "linux")]
    pub fn attach(child: &tokio::process::Child) -> Self {
        Self { pgid: child.id().and_then(|pid| i32::try_from(pid).ok()) }
    }

    /// Only the direct child is reachable here.
    #[cfg(not(any(windows, target_os = "linux")))]
    pub fn attach(_child: &tokio::process::Child) -> Self {
        Self::default()
    }

    /// Kills the whole tree, then (whatever happened there) the direct child.
    pub fn kill(&self, child: &mut tokio::process::Child) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(target_os = "linux")]
        if let Some(pgid) = self.pgid.filter(|p| *p > 1) {
            // SAFETY: plain syscall on a group this tree created; a gone group is just ESRCH.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = child.start_kill();
    }
}

/// A run dropped half-way (its task aborted, the app quitting) kills its child through
/// `kill_on_drop`, which reaches that one process: the rest of the group goes here. Only while the
/// leader is still there (running, or exited and not yet reaped) is its pid, the group's id,
/// certain to still name this group; a tree whose child was already waited on sends nothing.
#[cfg(target_os = "linux")]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.filter(|p| *p > 1) {
            // SAFETY: plain syscalls; signal 0 only checks that the leader still exists.
            unsafe {
                if libc::kill(pgid, 0) == 0 {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// An owned Job Object handle with `KILL_ON_JOB_CLOSE`; closed (killing what is left in it)
    /// on drop.
    pub struct Job(HANDLE);

    // SAFETY: a kernel handle is a process-wide value; the Job APIs used here are thread-safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A new job holding the process behind `process`.
        pub fn with_process(process: RawHandle) -> windows::core::Result<Self> {
            // SAFETY: plain Win32 calls; the handle is owned by `Job` from here on, so every
            // early return closes it through Drop.
            unsafe {
                let job = Job(CreateJobObjectW(None, PCWSTR::null())?);
                let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                    BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;
                AssignProcessToJobObject(job.0, HANDLE(process))?;
                Ok(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: `self.0` is a live job handle until Drop.
            if let Err(e) = unsafe { TerminateJobObject(self.0, 1) } {
                tracing::warn!(error = %e, "downloads: could not terminate the job object");
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed once, here.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Progress of an install, for the `tools-install-progress` event.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InstallProgress {
    /// `yt-dlp` or `ffmpeg`.
    pub tool: &'static str,
    /// `resolving`, `downloading`, `verifying`, `extracting` or `done`.
    pub stage: &'static str,
    pub received: u64,
    pub total: Option<u64>,
}

/// Installs or updates yt-dlp from `repo`'s latest release (stable or nightly). Answers the
/// version the new binary reports: proof it runs, not just that bytes arrived.
pub async fn install_ytdlp(
    data_dir: &Path,
    repo: &str,
    progress: &(dyn Fn(InstallProgress) + Send + Sync),
) -> Result<String, String> {
    if !MANAGED {
        return Err("not_managed".into());
    }
    let report = |stage, received, total| {
        progress(InstallProgress { tool: "yt-dlp", stage, received, total })
    };
    report("resolving", 0, None);
    let release = latest_release(repo).await?;
    let binary = release.asset(YTDLP_ASSET)?;
    let sums = parse_sums(&fetch_text(&release.asset(YTDLP_SUMS_ASSET)?.url).await?);
    let expected = sums.get(YTDLP_ASSET).ok_or("checksum_missing")?.clone();

    let target = bin_dir(data_dir).join(YTDLP_EXE);
    let part = part_path_for(&target);
    download_to(&binary.url, &part, &|received, total| report("downloading", received, total))
        .await?;
    report("verifying", 0, None);
    check_part(&part, &expected)?;
    std::fs::rename(&part, &target).map_err(|e| {
        // In use (a download is running on it) is the usual reason; the .part is ours to drop.
        let _ = std::fs::remove_file(&part);
        format!("could not replace yt-dlp: {e}")
    })?;
    report("done", 0, None);
    ytdlp_version(data_dir).await.ok_or_else(|| "installed_but_does_not_run".to_string())
}

/// Installs or updates ffmpeg (and ffprobe) from yt-dlp's FFmpeg-Builds.
pub async fn install_ffmpeg(
    data_dir: &Path,
    progress: &(dyn Fn(InstallProgress) + Send + Sync),
) -> Result<(), String> {
    if !MANAGED {
        return Err("not_managed".into());
    }
    let report = |stage, received, total| {
        progress(InstallProgress { tool: "ffmpeg", stage, received, total })
    };
    report("resolving", 0, None);
    let release = latest_release(FFMPEG_REPO).await?;
    let zip = release.asset(FFMPEG_ASSET)?;
    let sums = parse_sums(&fetch_text(&release.asset(FFMPEG_SUMS_ASSET)?.url).await?);
    let expected = sums.get(FFMPEG_ASSET).ok_or("checksum_missing")?.clone();

    let bin = bin_dir(data_dir);
    let zip_part = part_path_for(&bin.join(FFMPEG_ZIP));
    download_to(&zip.url, &zip_part, &|received, total| report("downloading", received, total))
        .await?;
    report("verifying", 0, None);
    check_part(&zip_part, &expected)?;

    report("extracting", 0, None);
    let extracted = {
        let (zip_part, bin) = (zip_part.clone(), bin.clone());
        tokio::task::spawn_blocking(move || {
            extract_exe(&zip_part, "ffmpeg.exe", &bin.join(FFMPEG_EXE))?;
            extract_exe(&zip_part, FFPROBE_EXE, &bin.join(FFPROBE_EXE))
        })
        .await
        .map_err(|e| format!("extraction task failed: {e}"))?
    };
    // Exactly the one file this function downloaded, by the path it built.
    let _ = std::fs::remove_file(&zip_part);
    extracted?;
    report("done", 0, None);
    Ok(())
}

/// One asset of a release: its name and download URL.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

impl Release {
    fn asset(&self, name: &str) -> Result<&Asset, String> {
        self.assets.iter().find(|a| a.name == name).ok_or_else(|| format!("asset_missing: {name}"))
    }
}

/// A GitHub `releases/latest` answer: the tag and every asset's name and URL.
pub fn parse_release(json: &serde_json::Value) -> Option<Release> {
    let tag = json.get("tag_name")?.as_str()?.to_string();
    let assets = json
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|a| {
            Some(Asset {
                name: a.get("name")?.as_str()?.to_string(),
                url: a.get("browser_download_url")?.as_str()?.to_string(),
            })
        })
        .collect();
    Some(Release { tag, assets })
}

async fn latest_release(repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let response = crate::http::client()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }
    let body = response.text().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let release = parse_release(&json).ok_or("bad_release")?;
    tracing::info!(repo, tag = %release.tag, "downloads: resolved tool release");
    Ok(release)
}

async fn fetch_text(url: &str) -> Result<String, String> {
    let response = crate::http::client()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

/// Streams `url` into `part` (created new, its folder too), reporting bytes as they come. A
/// failure deletes the `.part` it was writing.
async fn download_to(
    url: &str,
    part: &Path,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<(), String> {
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut response = crate::http::client()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }
    let total = response.content_length();
    let mut file = std::fs::File::create(part).map_err(|e| e.to_string())?;
    let mut received = 0u64;
    let result: Result<(), String> = async {
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            received += chunk.len() as u64;
            progress(received, total);
        }
        file.flush().map_err(|e| e.to_string())
    }
    .await;
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}

/// A checksum file (`<hex>  <name>`, `<hex> *<name>` for binary mode) as name → lowercase hex.
/// Lines that are not a 64-digit hash and a name are skipped.
pub fn parse_sums(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((hash, name)) = line.split_once(char::is_whitespace) else { continue };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name).trim();
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty() {
            out.insert(name.to_string(), hash.to_ascii_lowercase());
        }
    }
    out
}

/// The SHA-256 of the file at `path`, lowercase hex.
pub fn sha256_hex(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether the file at `path` hashes to `expected` (hex, any case). Reads only.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_hex(path).map_err(|e| e.to_string())?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err("checksum_mismatch".into())
    }
}

/// [`verify_sha256`] on a download's `.part`, deleting that file, and only a `.part`, when it
/// does not match.
fn check_part(part: &Path, expected: &str) -> Result<(), String> {
    let result = verify_sha256(part, expected);
    if result.is_err() && part.extension().is_some_and(|e| e == "part") {
        let _ = std::fs::remove_file(part);
    }
    result
}

/// Pulls the entry named `name` (in whatever folder of the zip) out to `target`, through a
/// `.part` and a rename.
fn extract_exe(zip_path: &Path, name: &str, target: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("not a readable zip: {e}"))?;
    let index = (0..archive.len()).find(|i| {
        archive.by_index(*i).ok().is_some_and(|entry| {
            let entry_name = entry.name().replace('\\', "/");
            entry.is_file() && entry_name.rsplit('/').next() == Some(name)
        })
    });
    let Some(index) = index else {
        return Err(format!("no {name} in the archive"));
    };
    let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
    let part = part_path_for(target);
    let copied = std::fs::File::create(&part)
        .and_then(|mut out| std::io::copy(&mut entry, &mut out).and_then(|_| out.flush()));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&part);
        return Err(format!("could not extract {name}: {e}"));
    }
    std::fs::rename(&part, target).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("could not replace {name}: {e}")
    })
}

/// `foo.exe` → `foo.exe.part`: appended, so two targets never share a temp name.
pub fn part_path_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // The async installers talk to GitHub; this suite never reaches the network or writes outside
    // its own temp folders. What is covered is everything they are built from.

    /// A shell that starts a grandchild and waits on it: killing the tree must take the grandchild
    /// too, not just the shell. Nothing is written to disk.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn killing_the_tree_reaches_the_grandchildren() {
        use tokio::io::AsyncBufReadExt;

        let mut command = hidden_command(Path::new("/bin/sh"));
        command
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        ProcessTree::prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child);
        let stdout = child.stdout.take().unwrap();
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let grandchild: u32 = lines.next_line().await.unwrap().unwrap().trim().parse().unwrap();

        tree.kill(&mut child);
        child.wait().await.unwrap();
        // Gone, or a zombie waiting for init to reap it: either way no longer running.
        let running = || {
            std::fs::read_to_string(format!("/proc/{grandchild}/stat")).is_ok_and(|stat| {
                !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z')
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while running() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!running(), "the grandchild {grandchild} outlived the kill");
    }

    /// Dropping the tree while its child still runs (a run dropped half-way) takes the group too.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_tree_reaches_the_grandchildren() {
        use tokio::io::AsyncBufReadExt;

        let mut command = hidden_command(Path::new("/bin/sh"));
        command
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        ProcessTree::prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child);
        let stdout = child.stdout.take().unwrap();
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let grandchild: u32 = lines.next_line().await.unwrap().unwrap().trim().parse().unwrap();

        drop(tree);
        let waited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        assert!(waited.is_ok(), "the shell outlived the dropped tree");
        let running = || {
            std::fs::read_to_string(format!("/proc/{grandchild}/stat")).is_ok_and(|stat| {
                !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z')
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while running() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!running(), "the grandchild {grandchild} outlived the dropped tree");
    }

    /// A process handle a test holds on to (so its pid cannot be reused meanwhile), closed on
    /// drop.
    #[cfg(windows)]
    struct Watched(windows::Win32::Foundation::HANDLE);

    #[cfg(windows)]
    impl Watched {
        fn open(pid: u32) -> Self {
            use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
            // SAFETY: plain Win32 call; the handle is closed in Drop.
            Watched(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("open it"))
        }

        /// Whether the process has ended, waiting up to `ms` for it to.
        fn ended_within(&self, ms: u32) -> bool {
            use windows::Win32::Foundation::WAIT_OBJECT_0;
            use windows::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: a live handle until Drop.
            let waited = unsafe { WaitForSingleObject(self.0, ms) };
            waited == WAIT_OBJECT_0
        }
    }

    #[cfg(windows)]
    impl Drop for Watched {
        fn drop(&mut self) {
            // SAFETY: closed once, here.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    /// PowerShell, in a tree, with a `ping` it started (which would run 30 s on its own) as the
    /// grandchild. Nothing is written to disk; a failing test leaves at most that ping behind,
    /// for at most those 30 s.
    #[cfg(windows)]
    struct Spawned {
        child: tokio::process::Child,
        tree: ProcessTree,
        grandchild: Watched,
        /// Kept open: a ping whose output pipe closed could die of that, not of the tree.
        _stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    }

    #[cfg(windows)]
    async fn spawn_with_grandchild() -> Spawned {
        use tokio::io::AsyncBufReadExt;

        const SCRIPT: &str = "$p = Start-Process ping -ArgumentList '-n','30','127.0.0.1' \
                              -NoNewWindow -PassThru; [Console]::Out.WriteLine($p.Id); \
                              [Console]::Out.Flush(); Start-Sleep 30";
        let mut command = hidden_command(Path::new("powershell.exe"));
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped());
        ProcessTree::prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child);
        assert!(tree.job.is_some(), "no job object");
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
        // Ping's own lines share the pipe (in the system's code page): the pid is the line that
        // is a bare number.
        let read_pid = async {
            let mut line = Vec::new();
            loop {
                line.clear();
                assert!(stdout.read_until(b'\n', &mut line).await.unwrap() > 0, "no pid");
                if let Ok(pid) = String::from_utf8_lossy(&line).trim().parse::<u32>() {
                    return pid;
                }
            }
        };
        let pid = tokio::time::timeout(Duration::from_secs(20), read_pid).await.expect("a pid");
        let grandchild = Watched::open(pid);
        assert!(!grandchild.ended_within(0), "the grandchild should be running");
        Spawned { child, tree, grandchild, _stdout: stdout }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn killing_the_tree_reaches_the_grandchildren_on_windows() {
        let mut run = spawn_with_grandchild().await;
        run.tree.kill(&mut run.child);
        let waited = tokio::time::timeout(Duration::from_secs(5), run.child.wait()).await;
        assert!(waited.is_ok(), "PowerShell outlived the kill");
        assert!(run.grandchild.ended_within(5000), "the grandchild outlived the kill");
    }

    /// Closing the job (the tree dropped: a run dropped half-way, the app exiting) kills what is
    /// in it, with nobody calling `kill`.
    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_the_tree_reaches_the_grandchildren_on_windows() {
        let mut run = spawn_with_grandchild().await;
        drop(run.tree);
        let waited = tokio::time::timeout(Duration::from_secs(5), run.child.wait()).await;
        assert!(waited.is_ok(), "PowerShell outlived the closed job");
        assert!(run.grandchild.ended_within(5000), "the grandchild outlived the closed job");
    }

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn checksum_files_parse_in_both_text_and_binary_mode() {
        let text = format!(
            "{HELLO_SHA256}  yt-dlp.exe\n\
             {}  *ffmpeg-master-latest-win64-gpl.zip\r\n\
             not a line\n\
             abc123  short.hash\n\
             {HELLO_SHA256}\n\
             \n",
            HELLO_SHA256.to_ascii_uppercase()
        );
        let sums = parse_sums(&text);
        assert_eq!(sums.len(), 2, "{sums:?}");
        assert_eq!(sums["yt-dlp.exe"], HELLO_SHA256);
        assert_eq!(sums["ffmpeg-master-latest-win64-gpl.zip"], HELLO_SHA256, "lowercased");
    }

    #[test]
    fn a_matching_file_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("yt-dlp.exe.part");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(sha256_hex(&file).unwrap(), HELLO_SHA256);
        assert_eq!(verify_sha256(&file, HELLO_SHA256), Ok(()));
        assert_eq!(verify_sha256(&file, &HELLO_SHA256.to_ascii_uppercase()), Ok(()));
        assert_eq!(check_part(&file, HELLO_SHA256), Ok(()));
        assert!(file.exists(), "a match keeps the file");
    }

    #[test]
    fn a_mismatch_deletes_only_its_own_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("yt-dlp.exe");
        std::fs::write(&target, b"the yt-dlp already installed").unwrap();
        let part = part_path_for(&target);
        std::fs::write(&part, b"tampered").unwrap();
        let other = dir.path().join("ffmpeg.exe.part");
        std::fs::write(&other, b"another download").unwrap();

        assert_eq!(check_part(&part, HELLO_SHA256), Err("checksum_mismatch".to_string()));
        assert!(!part.exists(), "the mismatching .part is gone");
        assert!(target.exists(), "the installed binary is untouched");
        assert!(other.exists(), "and so is every other file");
        assert!(dir.path().is_dir());
    }

    #[test]
    fn verify_alone_never_deletes_and_check_part_refuses_a_non_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("yt-dlp.exe");
        std::fs::write(&file, b"tampered").unwrap();
        assert!(verify_sha256(&file, HELLO_SHA256).is_err());
        assert!(file.exists());
        assert!(check_part(&file, HELLO_SHA256).is_err());
        assert!(file.exists(), "only a .part is ever deleted");
        assert!(verify_sha256(&dir.path().join("absent.part"), HELLO_SHA256).is_err());
    }

    #[test]
    fn a_release_answer_parses_into_its_tag_and_assets() {
        let json = serde_json::json!({
            "tag_name": "2026.09.30",
            "assets": [
                { "name": "yt-dlp.exe", "browser_download_url": "https://github.com/yt-dlp/yt-dlp/releases/download/2026.09.30/yt-dlp.exe" },
                { "name": "SHA2-256SUMS", "browser_download_url": "https://github.com/yt-dlp/yt-dlp/releases/download/2026.09.30/SHA2-256SUMS" },
                { "name": "broken" }
            ]
        });
        let release = parse_release(&json).unwrap();
        assert_eq!(release.tag, "2026.09.30");
        assert_eq!(release.assets.len(), 2);
        let sums = release.asset(YTDLP_SUMS_ASSET).unwrap();
        assert!(sums.url.contains("/2026.09.30/"), "the sums come from the same tag");
        assert!(release.asset("yt-dlp_macos").is_err());
        assert_eq!(parse_release(&serde_json::json!({ "message": "rate limited" })), None);
    }

    #[test]
    fn tool_paths_live_under_the_data_bin_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(bin_dir(dir.path()), dir.path().join("bin"));
        if MANAGED {
            assert_eq!(ytdlp_path(dir.path()), None, "nothing installed yet");
            assert!(!ffmpeg_present(dir.path()));
            let bin = bin_dir(dir.path());
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join(YTDLP_EXE), b"MZ").unwrap();
            std::fs::write(bin.join(FFMPEG_EXE), b"MZ").unwrap();
            assert_eq!(ytdlp_path(dir.path()), Some(bin.join("yt-dlp.exe")));
            assert_eq!(ffmpeg_dir(dir.path()), Some(bin));
        } else {
            assert_eq!(ffmpeg_dir(dir.path()), None, "the system's ffmpeg is found by yt-dlp");
        }
    }

    #[test]
    fn the_part_file_name_appends_rather_than_replacing_the_extension() {
        assert_eq!(
            part_path_for(Path::new("/bin/yt-dlp.exe")),
            PathBuf::from("/bin/yt-dlp.exe.part")
        );
        assert_eq!(
            part_path_for(Path::new("/bin/ffmpeg-download.zip")),
            PathBuf::from("/bin/ffmpeg-download.zip.part")
        );
    }

    #[test]
    fn create_no_window_matches_the_win32_constant() {
        assert_eq!(CREATE_NO_WINDOW, 0x08000000);
    }

    #[test]
    fn extraction_pulls_ffmpeg_and_ffprobe_out_of_their_nested_folder() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("archive.zip.part");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let entries: [(&str, &[u8]); 4] = [
                ("ffmpeg-master-latest-win64-gpl/README.txt", b"docs"),
                ("ffmpeg-master-latest-win64-gpl/bin/ffmpeg.exe", b"MZ ffmpeg"),
                ("ffmpeg-master-latest-win64-gpl/bin/ffprobe.exe", b"MZ ffprobe"),
                ("ffmpeg-master-latest-win64-gpl/bin/ffplay.exe", b"MZ ffplay"),
            ];
            for (name, body) in entries {
                writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        let ffmpeg = dir.path().join("ffmpeg.exe");
        extract_exe(&zip_path, "ffmpeg.exe", &ffmpeg).unwrap();
        extract_exe(&zip_path, "ffprobe.exe", &dir.path().join("ffprobe.exe")).unwrap();
        assert_eq!(std::fs::read(&ffmpeg).unwrap(), b"MZ ffmpeg");
        assert_eq!(std::fs::read(dir.path().join("ffprobe.exe")).unwrap(), b"MZ ffprobe");
        assert!(!part_path_for(&ffmpeg).exists(), "the .part is renamed away");
        assert!(!dir.path().join("ffplay.exe").exists(), "only what was asked for");
        let missing = extract_exe(&zip_path, "nope.exe", &dir.path().join("nope.exe"));
        assert!(missing.unwrap_err().contains("nope.exe"));
    }

    #[test]
    fn extracting_from_a_file_that_is_not_a_zip_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("not-a-zip.zip");
        std::fs::write(&fake, b"definitely not a zip").unwrap();
        assert!(extract_exe(&fake, "ffmpeg.exe", &dir.path().join("ffmpeg.exe")).is_err());
    }
}
