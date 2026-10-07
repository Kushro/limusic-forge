//! The pure half of driving yt-dlp, ported from PlaylistForge: the exact argument vector for a
//! download, and parsing what comes back on stdout/stderr. Nothing here spawns a process.
//!
//! yt-dlp's own progress bar rewrites one line with `\r` and ANSI codes, so three flags make the
//! output machine-readable: `--newline` (one line per update), `--progress-template` (our format,
//! behind [`PROGRESS_PREFIX`]) and `--print after_move:…` (the final path once post-processing has
//! moved the file, behind [`FINAL_PATH_PREFIX`]). `--print` implies `--quiet` and `--simulate`, so
//! `--no-simulate` and `--progress` have to come back explicitly; the tests pin that trio.
//!
//! The output template `%(title)s [%(id)s].%(ext)s` carries the video id in brackets: that marker
//! is how [`super::verify`] finds a file the user moved or renamed.
//!
//! Two departures from PF: `--embed-metadata` always (D22: title, artist and album go into the
//! file, so the local library reads them from tags rather than the file name), and `--cookies
//! <file>` with the session's cookies instead of `--cookies-from-browser`.

use std::path::{Path, PathBuf};

use super::settings::{AudioQuality, Format, ThumbnailMode, VideoQuality};

/// Prefix on every progress line we ask yt-dlp for.
pub const PROGRESS_PREFIX: &str = "PFPROGRESS:";
/// Prefix on the single final-path line.
pub const FINAL_PATH_PREFIX: &str = "PFPATH:";

/// `--progress-template`: download progress only, pipe-separated (a pipe never appears in
/// yt-dlp's percent, speed or eta strings).
pub const PROGRESS_TEMPLATE: &str =
    "download:PFPROGRESS:%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s";

/// `--print`: the path after the move, the only phase whose `%(filepath)s` is the file that is
/// there when the process exits.
pub const FINAL_PATH_TEMPLATE: &str = "after_move:PFPATH:%(filepath)s";

/// The file name; the folder comes from `--paths`.
pub const OUTPUT_TEMPLATE: &str = "%(title)s [%(id)s].%(ext)s";

/// Everything [`build_args`] needs, already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsSpec {
    pub video_id: String,
    pub format: Format,
    /// Only read for audio.
    pub audio_quality: AudioQuality,
    /// Only read for video.
    pub video_quality: VideoQuality,
    pub thumbnail_mode: ThumbnailMode,
    pub dest_dir: PathBuf,
    /// The folder holding ffmpeg, as `--ffmpeg-location`. `None` lets yt-dlp look on PATH, and
    /// say so plainly when a combination needs it and it is not there.
    pub ffmpeg_dir: Option<PathBuf>,
    /// A Netscape cookies file ([`super::cookies`]) as `--cookies`. `None` runs anonymous.
    pub cookies_file: Option<PathBuf>,
}

/// Whether `id` has the shape of a YouTube video id (`^[A-Za-z0-9_-]{11}$`). Only ids that pass
/// are queued or handed to yt-dlp, so nothing else ever ends up in [`video_url`].
pub fn is_valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A full watch URL: a bare id starting with `-` would read as a flag. Callers check the id with
/// [`is_valid_video_id`] first.
pub fn video_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// `-f`: the best audio-only stream (falling back to a muxed one) for audio; best video plus best
/// audio for video, height-capped on both halves when the quality says so.
pub fn format_selector(spec: &ArgsSpec) -> String {
    match spec.format {
        Format::Audio => "bestaudio/best".to_string(),
        Format::Video => match spec.video_quality.max_height() {
            None => "bv*+ba/b".to_string(),
            Some(height) => format!("bv*[height<={height}]+ba/b[height<={height}]"),
        },
    }
}

/// The argument vector after the program name, in a fixed order the tests assert verbatim.
pub fn build_args(spec: &ArgsSpec) -> Vec<String> {
    fn push(args: &mut Vec<String>, items: &[&str]) {
        args.extend(items.iter().map(|a| a.to_string()));
    }
    let mut args: Vec<String> = Vec::new();

    push(
        &mut args,
        &[
            "--newline",
            "--progress",
            "--no-simulate",
            "--progress-template",
            PROGRESS_TEMPLATE,
            "--print",
            FINAL_PATH_TEMPLATE,
            // A watch URL that sits in a playlist would otherwise pull the whole playlist.
            "--no-playlist",
            "--windows-filenames",
            "--no-warnings",
        ],
    );
    let dest = spec.dest_dir.to_string_lossy();
    push(&mut args, &["--paths", dest.as_ref(), "--output", OUTPUT_TEMPLATE]);
    push(&mut args, &["--format", format_selector(spec).as_str()]);

    match (spec.format, spec.audio_quality.mp3_bitrate()) {
        (Format::Audio, Some(bitrate)) => push(
            &mut args,
            &["--extract-audio", "--audio-format", "mp3", "--audio-quality", bitrate],
        ),
        // `best` with no `--audio-quality` is a codec copy into a proper container (webm →
        // .opus), which the thumbnail embedder accepts; never a re-encode.
        (Format::Audio, None) => push(&mut args, &["--extract-audio", "--audio-format", "best"]),
        (Format::Video, _) => push(&mut args, &["--merge-output-format", "mp4"]),
    }

    if spec.thumbnail_mode.embeds() {
        push(&mut args, &["--embed-thumbnail"]);
    }
    if spec.thumbnail_mode.writes_file() {
        push(&mut args, &["--write-thumbnail"]);
    }
    // D22: always.
    push(&mut args, &["--embed-metadata"]);

    if let Some(dir) = &spec.ffmpeg_dir {
        push(&mut args, &["--ffmpeg-location", dir.to_string_lossy().as_ref()]);
    }
    if let Some(file) = &spec.cookies_file {
        push(&mut args, &["--cookies", file.to_string_lossy().as_ref()]);
    }

    args.push(video_url(&spec.video_id));
    args
}

/// One parsed progress line.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressUpdate {
    /// 0.0..=100.0.
    pub percent: f64,
    /// yt-dlp's own string (`1.20MiB/s`), `None` while unknown.
    pub speed: Option<String>,
    /// `00:42`, `None` while unknown.
    pub eta: Option<String>,
}

/// A stdout line as a [`ProgressUpdate`], or `None` if it is not one of ours. Tolerates yt-dlp's
/// padding, `Unknown`/`N/A` fields and a trailing `\r` from a Windows pipe.
pub fn parse_progress_line(line: &str) -> Option<ProgressUpdate> {
    let rest = line.trim().strip_prefix(PROGRESS_PREFIX)?;
    let mut parts = rest.split('|');
    let percent = parts.next()?.trim().trim_end_matches('%').trim().parse::<f64>().ok()?;
    let speed = parts.next().and_then(unknown_to_none);
    let eta = parts.next().and_then(unknown_to_none);
    Some(ProgressUpdate { percent: percent.clamp(0.0, 100.0), speed, eta })
}

/// The `after_move` line's absolute path, or `None` if this is not that line.
pub fn parse_final_path_line(line: &str) -> Option<PathBuf> {
    let rest = line.trim().strip_prefix(FINAL_PATH_PREFIX)?.trim();
    if rest.is_empty() || rest == "NA" {
        return None;
    }
    Some(PathBuf::from(rest))
}

/// What a failed run's `error` column says: the first `ERROR:` line, else the last non-empty
/// line, else `None`; at most 400 characters.
pub fn extract_error_message(stderr: &str) -> Option<String> {
    const MAX_LEN: usize = 400;
    let first_error = stderr.lines().map(str::trim).find(|line| line.starts_with("ERROR:"));
    let chosen = first_error
        .or_else(|| stderr.lines().map(str::trim).filter(|line| !line.is_empty()).next_back())?;
    if chosen.is_empty() {
        return None;
    }
    Some(truncate_chars(chosen, MAX_LEN))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn unknown_to_none(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let unknown = ["", "N/A", "NA"].contains(&trimmed) || trimmed.starts_with("Unknown");
    if unknown {
        return None;
    }
    Some(trimmed.to_string())
}

/// The produced file's extension, lowercased: `downloads.container`.
pub fn container_of(path: &Path) -> Option<String> {
    path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_spec() -> ArgsSpec {
        ArgsSpec {
            video_id: "dQw4w9WgXcQ".to_string(),
            format: Format::Audio,
            audio_quality: AudioQuality::Mp3_320,
            video_quality: VideoQuality::Best,
            thumbnail_mode: ThumbnailMode::Embed,
            dest_dir: PathBuf::from(r"C:\Users\Someone\Music\LiMusic Forge"),
            ffmpeg_dir: Some(PathBuf::from(r"C:\Users\Someone\AppData\Roaming\forge\bin")),
            cookies_file: None,
        }
    }

    #[test]
    fn mp3_audio_args_are_exactly_this_vector() {
        assert_eq!(
            build_args(&audio_spec()),
            vec![
                "--newline",
                "--progress",
                "--no-simulate",
                "--progress-template",
                "download:PFPROGRESS:%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s",
                "--print",
                "after_move:PFPATH:%(filepath)s",
                "--no-playlist",
                "--windows-filenames",
                "--no-warnings",
                "--paths",
                r"C:\Users\Someone\Music\LiMusic Forge",
                "--output",
                "%(title)s [%(id)s].%(ext)s",
                "--format",
                "bestaudio/best",
                "--extract-audio",
                "--audio-format",
                "mp3",
                "--audio-quality",
                "320K",
                "--embed-thumbnail",
                "--embed-metadata",
                "--ffmpeg-location",
                r"C:\Users\Someone\AppData\Roaming\forge\bin",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            ]
        );
    }

    #[test]
    fn metadata_is_embedded_in_every_combination() {
        for format in Format::ALL {
            for mode in ThumbnailMode::ALL {
                let mut spec = audio_spec();
                spec.format = format;
                spec.thumbnail_mode = mode;
                let args = build_args(&spec);
                assert_eq!(
                    args.iter().filter(|a| *a == "--embed-metadata").count(),
                    1,
                    "{format:?}/{mode:?}"
                );
            }
        }
    }

    #[test]
    fn best_audio_normalizes_the_container_without_ever_asking_for_a_transcode() {
        let mut spec = audio_spec();
        spec.audio_quality = AudioQuality::Best;
        let args = build_args(&spec);
        let at = args.iter().position(|a| a == "--audio-format").expect("Best still extracts");
        assert_eq!(args[at + 1], "best");
        assert!(args.iter().any(|a| a == "--extract-audio"));
        assert!(!args.iter().any(|a| a == "--audio-quality"), "would re-encode: {args:?}");
        assert!(args.iter().any(|a| a == "bestaudio/best"));
    }

    #[test]
    fn cookies_are_only_passed_when_there_is_a_file() {
        let args = build_args(&audio_spec());
        assert!(!args.iter().any(|a| a == "--cookies"), "anonymous without a file");
        assert!(!args.iter().any(|a| a == "--cookies-from-browser"), "never the browser's");

        let mut spec = audio_spec();
        spec.cookies_file = Some(PathBuf::from(r"C:\forge\tmp\cookies-0123456789abcdef.txt"));
        let args = build_args(&spec);
        let at = args.iter().position(|a| a == "--cookies").expect("flag present with a file");
        assert_eq!(args[at + 1], r"C:\forge\tmp\cookies-0123456789abcdef.txt");
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn video_args_mux_to_mp4_and_cap_the_height_for_a_capped_quality() {
        let mut spec = audio_spec();
        spec.format = Format::Video;
        spec.video_quality = VideoQuality::P1080;
        let args = build_args(&spec);
        let value = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone());
        assert_eq!(value("--format").unwrap(), "bv*[height<=1080]+ba/b[height<=1080]");
        assert_eq!(value("--merge-output-format").unwrap(), "mp4");
        assert!(!args.iter().any(|a| a == "--extract-audio"), "video never extracts audio");
    }

    #[test]
    fn best_video_leaves_the_height_uncapped() {
        let mut spec = audio_spec();
        spec.format = Format::Video;
        spec.video_quality = VideoQuality::Best;
        assert_eq!(format_selector(&spec), "bv*+ba/b");
    }

    #[test]
    fn each_thumbnail_mode_maps_to_its_own_flag_combination() {
        let cases = [
            (ThumbnailMode::Embed, true, false),
            (ThumbnailMode::File, false, true),
            (ThumbnailMode::Both, true, true),
            (ThumbnailMode::None, false, false),
        ];
        for (mode, embed, file) in cases {
            let mut spec = audio_spec();
            spec.thumbnail_mode = mode;
            let args = build_args(&spec);
            assert_eq!(args.iter().any(|a| a == "--embed-thumbnail"), embed, "{mode:?}");
            assert_eq!(args.iter().any(|a| a == "--write-thumbnail"), file, "{mode:?}");
        }
    }

    #[test]
    fn ffmpeg_location_is_omitted_when_there_is_no_managed_ffmpeg() {
        let mut spec = audio_spec();
        spec.ffmpeg_dir = None;
        assert!(!build_args(&spec).iter().any(|a| a == "--ffmpeg-location"));
    }

    #[test]
    fn the_url_is_always_the_last_argument() {
        let args = build_args(&audio_spec());
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn progress_lines_parse_with_padding_and_a_trailing_carriage_return() {
        let update = parse_progress_line("PFPROGRESS:  12.3%|   1.20MiB/s|00:42\r").unwrap();
        assert_eq!(update.percent, 12.3);
        assert_eq!(update.speed.as_deref(), Some("1.20MiB/s"));
        assert_eq!(update.eta.as_deref(), Some("00:42"));
    }

    #[test]
    fn progress_lines_report_unknown_speed_and_eta_as_none() {
        let update = parse_progress_line("PFPROGRESS:   0.0%|Unknown B/s|Unknown").unwrap();
        assert_eq!(update.percent, 0.0);
        assert_eq!(update.speed, None);
        assert_eq!(update.eta, None);
        let na = parse_progress_line("PFPROGRESS: 100.0%|N/A|N/A").unwrap();
        assert_eq!(na.percent, 100.0);
        assert_eq!(na.speed, None);
    }

    #[test]
    fn lines_that_are_not_progress_lines_parse_to_none() {
        assert!(parse_progress_line("[download] Destination: Song [abc].webm").is_none());
        assert!(parse_progress_line("").is_none());
        assert!(parse_progress_line("PFPATH:C:\\dl\\Song [abc].mp3").is_none());
        assert!(parse_progress_line("PFPROGRESS:not-a-number|1MiB/s|00:01").is_none());
    }

    #[test]
    fn a_percent_beyond_the_range_is_clamped() {
        assert_eq!(parse_progress_line("PFPROGRESS:104.0%|1MiB/s|00:00").unwrap().percent, 100.0);
    }

    #[test]
    fn the_final_path_line_parses_and_is_told_apart_from_progress() {
        let line = r"PFPATH:C:\Users\Someone\Music\LiMusic Forge\Song [dQw4w9WgXcQ].mp3";
        assert_eq!(
            parse_final_path_line(line).unwrap(),
            PathBuf::from(r"C:\Users\Someone\Music\LiMusic Forge\Song [dQw4w9WgXcQ].mp3")
        );
        assert!(parse_final_path_line("PFPROGRESS:  12.3%|1MiB/s|00:42").is_none());
        assert!(parse_final_path_line("PFPATH:").is_none());
        assert!(parse_final_path_line("PFPATH:NA").is_none(), "NA = unresolved template field");
    }

    #[test]
    fn error_extraction_prefers_the_first_error_line() {
        let stderr = "WARNING: cosmetic\nERROR: [youtube] abc: Video unavailable\ntrailing noise\n";
        assert_eq!(
            extract_error_message(stderr).as_deref(),
            Some("ERROR: [youtube] abc: Video unavailable")
        );
    }

    #[test]
    fn error_extraction_falls_back_to_the_last_meaningful_line() {
        let stderr = "first line\nlast meaningful line\n\n";
        assert_eq!(extract_error_message(stderr).as_deref(), Some("last meaningful line"));
        assert_eq!(extract_error_message("   \n\n"), None);
        assert_eq!(extract_error_message(""), None);
    }

    #[test]
    fn error_extraction_truncates_a_wall_of_text() {
        let message = extract_error_message(&format!("ERROR: {}", "x".repeat(5000))).unwrap();
        assert_eq!(message.chars().count(), 401, "400 characters plus the ellipsis");
        assert!(message.ends_with('…'));
    }

    #[test]
    fn container_of_lowercases_the_extension() {
        assert_eq!(container_of(Path::new(r"C:\dl\Song [abc].MP3")).as_deref(), Some("mp3"));
        assert_eq!(container_of(Path::new(r"C:\dl\Song [abc].m4a")).as_deref(), Some("m4a"));
        assert_eq!(container_of(Path::new(r"C:\dl\no-extension")), None);
    }

    #[test]
    fn only_eleven_url_safe_characters_make_a_video_id() {
        for ok in ["dQw4w9WgXcQ", "-abcdefghij", "a_b-C_d-E_9", "___________"] {
            assert!(is_valid_video_id(ok), "{ok}");
        }
        for bad in [
            "",
            "dQw4w9WgXc",
            "dQw4w9WgXcQQ",
            "--version=1",
            "dQw4w9WgXc ",
            "dQw4w9WgX/Q",
            "dQw4w9WgX&Q",
            "dQw4w9WgX?Q",
            "local:a.mp3",
            "dQw4w9WgXcé",
            "ñQw4w9WgXc",
        ] {
            assert!(!is_valid_video_id(bad), "{bad:?}");
        }
    }
}
