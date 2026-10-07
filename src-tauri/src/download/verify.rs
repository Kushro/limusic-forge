//! "Is the downloaded file still there?", ported from PlaylistForge.
//!
//! A `downloads` row is the app's memory of a download; the file belongs to the user, who may
//! move, rename or delete it while the app is closed. [`verify_for_video_ids`] re-checks the
//! `available` and `missing` rows of the videos asked about: a file at the recorded path stays (or
//! becomes again) `available`; one found elsewhere in the row's own `dest_dir` by its
//! `[{video_id}]` marker is adopted, path and all; otherwise the row goes `missing`. `queued`,
//! `running` and `error` rows are left alone: a file check has nothing to say about them.
//!
//! The rescan stays inside the recorded `dest_dir`, never above it, bounded in depth
//! ([`MAX_SCAN_DEPTH`]) and entries ([`MAX_SCAN_ENTRIES`]), and never follows links. Files that
//! only accompany a download (the written thumbnail, yt-dlp's `.part`/`.ytdl`) are skipped, and
//! so are extensions that belong unambiguously to the other format: a video downloaded both ways
//! shares one folder and one marker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::{Db, DownloadRow};

use super::settings::Format;

pub const MAX_SCAN_DEPTH: usize = 6;
pub const MAX_SCAN_ENTRIES: usize = 20_000;

const NON_MEDIA_EXTENSIONS: &[&str] =
    &["part", "ytdl", "jpg", "jpeg", "png", "webp", "gif", "temp", "tmp"];
/// `webm`/`mka` are in neither list: a `bestaudio` download can land as `.webm`.
const VIDEO_ONLY_EXTENSIONS: &[&str] =
    &["mp4", "m4v", "mkv", "mov", "avi", "flv", "wmv", "mpg", "mpeg", "ts"];
const AUDIO_ONLY_EXTENSIONS: &[&str] =
    &["mp3", "m4a", "opus", "ogg", "oga", "aac", "flac", "wav", "aiff", "alac"];

fn extension_conflicts_with(format: Format, extension: &str) -> bool {
    match format {
        Format::Audio => VIDEO_ONLY_EXTENSIONS.contains(&extension),
        Format::Video => AUDIO_ONLY_EXTENSIONS.contains(&extension),
    }
}

/// Re-checks the `available`/`missing` rows of `video_ids`, then answers every row of them
/// (touched or not) grouped by video. Videos with no row are absent from the map.
pub fn verify_for_video_ids(
    db: &Db,
    video_ids: &[String],
    now: i64,
) -> HashMap<String, Vec<DownloadRow>> {
    let rows = db.downloads_for(video_ids);
    reconcile(db, &rows, now);
    let mut grouped: HashMap<String, Vec<DownloadRow>> = HashMap::new();
    for row in db.downloads_for(video_ids) {
        grouped.entry(row.video_id.clone()).or_default().push(row);
    }
    grouped
}

/// The startup pass: every `available`/`missing` row. Answers how many changed state.
pub fn verify_all(db: &Db, now: i64) -> usize {
    let ids = db.verifiable_download_ids();
    let before = db.downloads_for(&ids);
    reconcile(db, &before, now);
    let after = db.downloads_for(&ids);
    after
        .iter()
        .filter(|row| {
            !before.iter().any(|b| {
                b.video_id == row.video_id
                    && b.format == row.format
                    && b.status == row.status
                    && b.file_path == row.file_path
            })
        })
        .count()
}

fn reconcile(db: &Db, rows: &[DownloadRow], now: i64) {
    for row in rows {
        if !matches!(row.status.as_str(), "available" | "missing") {
            continue;
        }
        let Some(format) = Format::parse(&row.format) else { continue };
        // The files this video's other format owns: deleting just the .mp3 of a video kept both
        // ways must not point the audio row at the .mp4.
        let siblings: Vec<PathBuf> = rows
            .iter()
            .filter(|o| o.video_id == row.video_id && o.format != row.format)
            .filter_map(|o| o.file_path.as_deref().map(PathBuf::from))
            .collect();
        reconcile_row(db, row, format, &siblings, now);
    }
}

/// The recorded path first, then the marker rescan, then `missing`.
fn reconcile_row(db: &Db, row: &DownloadRow, format: Format, siblings: &[PathBuf], now: i64) {
    if let Some(path) = row.file_path.as_deref() {
        if let Some(size) = file_size(Path::new(path)) {
            // Still there: re-stamp, with the real size, and back to `available` if it was
            // `missing`. The recorded container stays.
            db.mark_download_found(&row.video_id, &row.format, path, Some(size), None, now);
            return;
        }
    }
    let found = find_by_marker(
        Path::new(&row.dest_dir),
        &row.video_id,
        format,
        row.container.as_deref(),
        siblings,
    );
    if let Some(found) = found {
        // The adopted file may carry another extension: the container follows the real file.
        let container = super::ytdlp_args::container_of(&found);
        db.mark_download_found(
            &row.video_id,
            &row.format,
            &found.to_string_lossy(),
            file_size(&found),
            container.as_deref(),
            now,
        );
        return;
    }
    // Gone. Idempotent for a row already `missing`.
    db.mark_download_missing(&row.video_id, &row.format, now);
}

fn file_size(path: &Path) -> Option<i64> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_file().then(|| metadata.len() as i64)
}

/// The best file under `root` whose name carries `[{video_id}]`: one with the recorded
/// extension (`preferred_container`) wins, else the largest. Never a path in `exclude`, a
/// companion file, or the other format's media.
pub fn find_by_marker(
    root: &Path,
    video_id: &str,
    format: Format,
    preferred_container: Option<&str>,
    exclude: &[PathBuf],
) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let marker = format!("[{video_id}]");
    let preferred = preferred_container.map(|c| c.to_ascii_lowercase());

    let mut best: Option<(PathBuf, u64)> = None;
    let mut preferred_hit: Option<PathBuf> = None;
    let mut examined = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            examined += 1;
            if examined > MAX_SCAN_ENTRIES {
                return preferred_hit.or(best.map(|(path, _)| path));
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_dir() {
                if depth < MAX_SCAN_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            // Links are skipped, not followed: a loop back up would defeat the depth cap.
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !name.contains(&marker) {
                continue;
            }
            let extension = super::ytdlp_args::container_of(&path);
            let ext = extension.as_deref().unwrap_or("");
            if NON_MEDIA_EXTENSIONS.contains(&ext) || extension_conflicts_with(format, ext) {
                continue;
            }
            if exclude.iter().any(|claimed| claimed == &path) {
                continue;
            }
            if preferred.as_deref().is_some_and(|p| p == ext) && preferred_hit.is_none() {
                preferred_hit = Some(path.clone());
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if best.as_ref().map_or(true, |(_, best_size)| size > *best_size) {
                best = Some((path, size));
            }
        }
    }
    preferred_hit.or(best.map(|(path, _)| path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewDownload, VideoMeta};

    const NOW: i64 = 1_784_000_000;
    const LATER: i64 = NOW + 3600;

    fn db() -> Db {
        Db::open(Path::new(":memory:")).unwrap()
    }

    fn enqueue(db: &Db, video_id: &str, format: &str, dest_dir: &Path) {
        let dest = dest_dir.to_string_lossy();
        db.enqueue_download(
            &NewDownload {
                video: VideoMeta {
                    video_id,
                    title: Some("Title"),
                    channel: None,
                    duration_s: None,
                },
                format,
                requested_quality: "best",
                thumbnail_mode: "embed",
                dest_dir: &dest,
                redownload: false,
            },
            NOW,
        )
        .unwrap();
    }

    /// One `available` row pointing at `file`; `dest_dir` is always a test-owned temp folder.
    fn seed_available(db: &Db, video_id: &str, format: &str, dest_dir: &Path, file: &Path) {
        enqueue(db, video_id, format, dest_dir);
        assert!(db.mark_download_running(video_id, format));
        let container = super::super::ytdlp_args::container_of(file);
        let path = file.to_string_lossy();
        assert!(db.mark_download_available(
            video_id,
            format,
            &path,
            Some(1),
            container.as_deref(),
            NOW
        ));
    }

    fn row(db: &Db, video_id: &str, format: &str) -> DownloadRow {
        db.downloads_for(&[video_id.to_owned()]).into_iter().find(|r| r.format == format).unwrap()
    }

    #[test]
    fn a_file_that_is_still_there_stays_available_and_refreshes_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Song [vid1].mp3");
        std::fs::write(&file, b"0123456789").unwrap();
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &file);

        let grouped = verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        let r = &grouped["vid1"][0];
        assert_eq!(r.status, "available");
        assert_eq!(r.file_size_bytes, Some(10), "the size on disk wins");
        assert_eq!(r.last_verified_at.as_deref(), Some(crate::db::rfc3339_utc(LATER).as_str()));
        assert_eq!(r.container.as_deref(), Some("mp3"));
    }

    #[test]
    fn a_deleted_file_flips_the_row_to_missing_but_keeps_its_last_known_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Song [vid1].mp3");
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &file);

        let grouped = verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        let r = &grouped["vid1"][0];
        assert_eq!(r.status, "missing");
        assert_eq!(r.file_path.as_deref(), Some(file.to_string_lossy().as_ref()));
    }

    #[test]
    fn a_file_moved_into_a_subfolder_is_found_by_its_marker_and_the_path_follows() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("Song [vid1].mp3");
        let sub = dir.path().join("Rock").join("2026");
        std::fs::create_dir_all(&sub).unwrap();
        let moved = sub.join("01 - Song (remastered) [vid1].mp3");
        std::fs::write(&moved, b"abcdefg").unwrap();
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &original);

        let grouped = verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        let r = &grouped["vid1"][0];
        assert_eq!(r.status, "available", "a moved file is not a missing one");
        assert_eq!(r.file_path.as_deref(), Some(moved.to_string_lossy().as_ref()));
        assert_eq!(r.file_size_bytes, Some(7));
    }

    #[test]
    fn a_missing_row_returns_to_available_once_the_file_reappears() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Song [vid1].mp3");
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &file);
        verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        assert_eq!(row(&db, "vid1", "audio").status, "missing");

        std::fs::write(&file, b"restored from the recycle bin").unwrap();
        let grouped = verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        assert_eq!(grouped["vid1"][0].status, "available");
    }

    #[test]
    fn queued_and_error_rows_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let db = db();
        enqueue(&db, "vid1", "audio", dir.path());
        enqueue(&db, "vid2", "audio", dir.path());
        assert!(db.mark_download_running("vid2", "audio"));
        assert!(db.mark_download_error("vid2", "audio", "boom"));

        let grouped = verify_for_video_ids(&db, &["vid1".to_string(), "vid2".to_string()], LATER);
        assert_eq!(grouped["vid1"][0].status, "queued");
        assert_eq!(grouped["vid2"][0].status, "error");
        assert_eq!(grouped["vid2"][0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn videos_with_no_row_do_not_appear() {
        let db = db();
        assert!(verify_for_video_ids(&db, &["vid1".to_string()], NOW).is_empty());
        assert!(verify_for_video_ids(&db, &[], NOW).is_empty());
    }

    #[test]
    fn the_startup_pass_reconciles_every_row_with_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("Kept [vid1].m4a");
        std::fs::write(&kept, b"audio").unwrap();
        let gone = dir.path().join("Gone [vid2].m4a");
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &kept);
        seed_available(&db, "vid2", "audio", dir.path(), &gone);
        enqueue(&db, "vid3", "audio", dir.path());

        assert_eq!(verify_all(&db, LATER), 1, "only vid2 changed state");
        assert_eq!(row(&db, "vid1", "audio").status, "available");
        assert_eq!(row(&db, "vid2", "audio").status, "missing");
        assert_eq!(row(&db, "vid3", "audio").status, "queued");
        assert_eq!(verify_all(&db, LATER), 0, "a second pass changes nothing");
    }

    #[test]
    fn the_marker_scan_ignores_the_thumbnail_and_part_files_that_share_the_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Song [vid1].webp"), b"cover art, bigger than the audio")
            .unwrap();
        std::fs::write(dir.path().join("Song [vid1].mp3.part"), b"interrupted bytes").unwrap();
        let real = dir.path().join("Song [vid1].mp3");
        std::fs::write(&real, b"tiny").unwrap();
        let found = find_by_marker(dir.path(), "vid1", Format::Audio, Some("mp3"), &[]).unwrap();
        assert_eq!(found, real);
    }

    #[test]
    fn the_marker_scan_prefers_the_recorded_container_over_the_bigger_file() {
        let dir = tempfile::tempdir().unwrap();
        let m4a = dir.path().join("Song [vid1].m4a");
        std::fs::write(&m4a, b"a much larger leftover from an earlier run").unwrap();
        let mp3 = dir.path().join("Song [vid1].mp3");
        std::fs::write(&mp3, b"small").unwrap();
        assert_eq!(find_by_marker(dir.path(), "vid1", Format::Audio, Some("mp3"), &[]), Some(mp3));
        assert_eq!(find_by_marker(dir.path(), "vid1", Format::Audio, None, &[]), Some(m4a));
    }

    #[test]
    fn the_marker_scan_matches_the_exact_id_and_not_a_prefix_of_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Other [vid10].mp3"), b"different video").unwrap();
        assert_eq!(find_by_marker(dir.path(), "vid1", Format::Audio, Some("mp3"), &[]), None);
    }

    #[test]
    fn the_marker_scan_on_a_folder_that_does_not_exist_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("nope");
        assert_eq!(find_by_marker(&absent, "vid1", Format::Audio, None, &[]), None);
    }

    #[test]
    fn the_marker_scan_stops_at_the_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for level in 0..(MAX_SCAN_DEPTH + 3) {
            deep = deep.join(format!("level{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Song [vid1].mp3"), b"buried too deep").unwrap();
        assert_eq!(find_by_marker(dir.path(), "vid1", Format::Audio, Some("mp3"), &[]), None);
    }

    /// A video downloaded both ways shares a folder and a marker: deleting the .mp3 must flip
    /// the audio row to `missing`, never point it at the video row's .mp4.
    #[test]
    fn deleting_one_formats_file_never_adopts_the_other_formats_file() {
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("Title [vid1].mp4");
        std::fs::write(&mp4, b"the intact video file").unwrap();
        let mp3 = dir.path().join("Title [vid1].mp3"); // never written: "deleted"
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &mp3);
        seed_available(&db, "vid1", "video", dir.path(), &mp4);

        verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        let audio = row(&db, "vid1", "audio");
        let video = row(&db, "vid1", "video");
        assert_eq!(audio.status, "missing", "the audio row must not adopt the video file");
        assert_eq!(audio.container.as_deref(), Some("mp3"));
        assert_eq!(video.status, "available");
        assert_eq!(video.file_path.as_deref(), Some(mp4.to_string_lossy().as_ref()));
    }

    #[test]
    fn the_marker_scan_never_crosses_extension_families() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Title [vid1].mp4"), b"unclaimed video").unwrap();
        assert_eq!(find_by_marker(dir.path(), "vid1", Format::Audio, Some("mp3"), &[]), None);
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("Title [vid1].flac"), b"unclaimed audio").unwrap();
        assert_eq!(find_by_marker(other.path(), "vid1", Format::Video, Some("mp4"), &[]), None);
    }

    #[test]
    fn a_relocated_webm_is_adopted_by_an_audio_row_and_refreshes_its_container() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("Song [vid1].opus");
        let moved = dir.path().join("Song moved [vid1].webm");
        std::fs::write(&moved, b"opus in webm").unwrap();
        let db = db();
        seed_available(&db, "vid1", "audio", dir.path(), &original);

        let grouped = verify_for_video_ids(&db, &["vid1".to_string()], LATER);
        let r = &grouped["vid1"][0];
        assert_eq!(r.status, "available");
        assert_eq!(r.file_path.as_deref(), Some(moved.to_string_lossy().as_ref()));
        assert_eq!(r.container.as_deref(), Some("webm"));
    }
}
