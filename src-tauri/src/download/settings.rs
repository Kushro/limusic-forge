//! The Downloads defaults, as string values in the `settings` table under PlaylistForge's keys
//! (`downloads.*`), plus two of our own: where the cookies come from and which yt-dlp build to
//! install. Each knob is a getter with a documented default: a missing key, a value from an older
//! build or a hand edit all read as the default rather than an error the UI would have to handle.
//!
//! Every queued row stores its own resolved quality and thumbnail mode, so changing a default here
//! never rewrites what is already in the queue.
//!
//! PF's `CookiesBrowser` (`--cookies-from-browser`) is gone: the cookies are the app's own login
//! session, written out per run by [`super::cookies`].

use std::path::{Path, PathBuf};

use crate::db::Db;

pub const DOWNLOADS_DIR_KEY: &str = "downloads.dir";
pub const DEFAULT_FORMAT_KEY: &str = "downloads.default_format";
pub const AUDIO_QUALITY_KEY: &str = "downloads.audio_quality";
pub const VIDEO_QUALITY_KEY: &str = "downloads.video_quality";
pub const THUMBNAIL_MODE_KEY: &str = "downloads.thumbnail_mode";
pub const COOKIES_KEY: &str = "downloads.cookies";
pub const YTDLP_CHANNEL_KEY: &str = "downloads.ytdlp_channel";

/// Every key above, for the `UI_SETTINGS` allow-list test.
#[cfg(test)]
pub const KEYS: [&str; 7] = [
    DOWNLOADS_DIR_KEY,
    DEFAULT_FORMAT_KEY,
    AUDIO_QUALITY_KEY,
    VIDEO_QUALITY_KEY,
    THUMBNAIL_MODE_KEY,
    COOKIES_KEY,
    YTDLP_CHANNEL_KEY,
];

/// What a download produces. Stored as `audio` / `video`, the vocabulary the `downloads` table's
/// CHECK allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Audio,
    Video,
}

impl Format {
    pub const ALL: [Self; 2] = [Format::Audio, Format::Video];

    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Audio => "audio",
            Format::Video => "video",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == raw)
    }
}

/// `Best` keeps the best audio-only stream YouTube serves (Opus or M4A) without re-encoding; the
/// `Mp3_*` ones transcode, for players that read nothing else.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioQuality {
    Best,
    Mp3_320,
    Mp3_256,
    Mp3_192,
}

impl AudioQuality {
    pub const ALL: [Self; 4] =
        [AudioQuality::Best, AudioQuality::Mp3_320, AudioQuality::Mp3_256, AudioQuality::Mp3_192];

    pub fn as_str(&self) -> &'static str {
        match self {
            AudioQuality::Best => "best",
            AudioQuality::Mp3_320 => "mp3_320",
            AudioQuality::Mp3_256 => "mp3_256",
            AudioQuality::Mp3_192 => "mp3_192",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == raw)
    }

    /// yt-dlp's `--audio-quality` for the mp3 variants; `None` for `Best`, which never transcodes.
    pub fn mp3_bitrate(&self) -> Option<&'static str> {
        match self {
            AudioQuality::Best => None,
            AudioQuality::Mp3_320 => Some("320K"),
            AudioQuality::Mp3_256 => Some("256K"),
            AudioQuality::Mp3_192 => Some("192K"),
        }
    }
}

/// `Best` takes the best video and audio there are; the others cap the picture height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoQuality {
    Best,
    P1080,
    P720,
}

impl VideoQuality {
    pub const ALL: [Self; 3] = [VideoQuality::Best, VideoQuality::P1080, VideoQuality::P720];

    pub fn as_str(&self) -> &'static str {
        match self {
            VideoQuality::Best => "best",
            VideoQuality::P1080 => "1080p",
            VideoQuality::P720 => "720p",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == raw)
    }

    pub fn max_height(&self) -> Option<u32> {
        match self {
            VideoQuality::Best => None,
            VideoQuality::P1080 => Some(1080),
            VideoQuality::P720 => Some(720),
        }
    }
}

/// The cover: inside the file (`Embed`), beside it (`File`), both, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbnailMode {
    Embed,
    File,
    Both,
    None,
}

impl ThumbnailMode {
    pub const ALL: [Self; 4] =
        [ThumbnailMode::Embed, ThumbnailMode::File, ThumbnailMode::Both, ThumbnailMode::None];

    pub fn as_str(&self) -> &'static str {
        match self {
            ThumbnailMode::Embed => "embed",
            ThumbnailMode::File => "file",
            ThumbnailMode::Both => "both",
            ThumbnailMode::None => "none",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == raw)
    }

    pub fn embeds(&self) -> bool {
        matches!(self, ThumbnailMode::Embed | ThumbnailMode::Both)
    }

    pub fn writes_file(&self) -> bool {
        matches!(self, ThumbnailMode::File | ThumbnailMode::Both)
    }
}

/// Whether yt-dlp gets the signed-in session's cookies (a Premium account unlocks renditions an
/// anonymous request never sees) or runs anonymous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CookiesMode {
    Session,
    None,
}

impl CookiesMode {
    pub const ALL: [Self; 2] = [CookiesMode::Session, CookiesMode::None];

    pub fn as_str(&self) -> &'static str {
        match self {
            CookiesMode::Session => "session",
            CookiesMode::None => "none",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == raw)
    }
}

/// Which yt-dlp builds "Install / update" fetches: the tagged releases, or the nightly builds that
/// carry YouTube fixes days earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YtdlpChannel {
    Stable,
    Nightly,
}

impl YtdlpChannel {
    pub const ALL: [Self; 2] = [YtdlpChannel::Stable, YtdlpChannel::Nightly];

    pub fn as_str(&self) -> &'static str {
        match self {
            YtdlpChannel::Stable => "stable",
            YtdlpChannel::Nightly => "nightly",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == raw)
    }

    /// The GitHub repository whose releases this channel installs from.
    pub fn repo(&self) -> &'static str {
        match self {
            YtdlpChannel::Stable => "yt-dlp/yt-dlp",
            YtdlpChannel::Nightly => "yt-dlp/yt-dlp-nightly-builds",
        }
    }
}

pub const DEFAULT_FORMAT: Format = Format::Audio;
pub const DEFAULT_AUDIO_QUALITY: AudioQuality = AudioQuality::Mp3_320;
pub const DEFAULT_VIDEO_QUALITY: VideoQuality = VideoQuality::Best;
pub const DEFAULT_THUMBNAIL_MODE: ThumbnailMode = ThumbnailMode::Embed;
pub const DEFAULT_COOKIES: CookiesMode = CookiesMode::Session;
pub const DEFAULT_YTDLP_CHANNEL: YtdlpChannel = YtdlpChannel::Stable;

/// The folder name under the user's Music folder.
const MUSIC_SUBDIR: &str = "LiMusic Forge";

/// Where downloads go when no folder is set: `<Music>/LiMusic Forge`, PlaylistForge's
/// `<Downloads>/PlaylistForge` moved to the folder a music library lives in. A portable copy
/// keeps them in its own `data/downloads`, like everything else it writes, and so does a system
/// that reports no Music folder.
pub fn default_dir(data_dir: &Path) -> PathBuf {
    default_dir_from(crate::paths::is_portable(), dirs::audio_dir(), data_dir)
}

fn default_dir_from(portable: bool, music: Option<PathBuf>, data_dir: &Path) -> PathBuf {
    match music.filter(|m| !portable && m.is_absolute()) {
        Some(music) => music.join(MUSIC_SUBDIR),
        None => data_dir.join("downloads"),
    }
}

/// The stored folder when it is an absolute path, `default` otherwise (a cleared field stores
/// an empty string).
pub fn resolve_dir(stored: Option<&str>, default: PathBuf) -> PathBuf {
    match stored.map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => default,
    }
}

pub fn downloads_dir(db: &Db, data_dir: &Path) -> PathBuf {
    resolve_dir(db.get_setting(DOWNLOADS_DIR_KEY).as_deref(), default_dir(data_dir))
}

pub fn default_format(db: &Db) -> Format {
    read(db, DEFAULT_FORMAT_KEY, Format::parse).unwrap_or(DEFAULT_FORMAT)
}

pub fn audio_quality(db: &Db) -> AudioQuality {
    read(db, AUDIO_QUALITY_KEY, AudioQuality::parse).unwrap_or(DEFAULT_AUDIO_QUALITY)
}

pub fn video_quality(db: &Db) -> VideoQuality {
    read(db, VIDEO_QUALITY_KEY, VideoQuality::parse).unwrap_or(DEFAULT_VIDEO_QUALITY)
}

pub fn thumbnail_mode(db: &Db) -> ThumbnailMode {
    read(db, THUMBNAIL_MODE_KEY, ThumbnailMode::parse).unwrap_or(DEFAULT_THUMBNAIL_MODE)
}

pub fn cookies(db: &Db) -> CookiesMode {
    read(db, COOKIES_KEY, CookiesMode::parse).unwrap_or(DEFAULT_COOKIES)
}

pub fn ytdlp_channel(db: &Db) -> YtdlpChannel {
    read(db, YTDLP_CHANNEL_KEY, YtdlpChannel::parse).unwrap_or(DEFAULT_YTDLP_CHANNEL)
}

/// The quality string a new row of `format` records: the audio or the video setting.
pub fn requested_quality(db: &Db, format: Format) -> &'static str {
    match format {
        Format::Audio => audio_quality(db).as_str(),
        Format::Video => video_quality(db).as_str(),
    }
}

fn read<T>(db: &Db, key: &str, parse: fn(&str) -> Option<T>) -> Option<T> {
    db.get_setting(key).and_then(|raw| parse(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn every_enum_round_trips_through_its_string_form() {
        for f in Format::ALL {
            assert_eq!(Format::parse(f.as_str()), Some(f));
        }
        for q in AudioQuality::ALL {
            assert_eq!(AudioQuality::parse(q.as_str()), Some(q));
        }
        for q in VideoQuality::ALL {
            assert_eq!(VideoQuality::parse(q.as_str()), Some(q));
        }
        for m in ThumbnailMode::ALL {
            assert_eq!(ThumbnailMode::parse(m.as_str()), Some(m));
        }
        for m in CookiesMode::ALL {
            assert_eq!(CookiesMode::parse(m.as_str()), Some(m));
        }
        for c in YtdlpChannel::ALL {
            assert_eq!(YtdlpChannel::parse(c.as_str()), Some(c));
        }
        assert_eq!(AudioQuality::parse("mp3_128"), None);
        assert_eq!(VideoQuality::parse("4k"), None);
        assert_eq!(ThumbnailMode::parse("EMBED"), None, "case-sensitive on purpose");
        assert_eq!(CookiesMode::parse("firefox"), None, "no browser cookies any more");
    }

    #[test]
    fn quality_helpers_match_the_yt_dlp_vocabulary() {
        assert_eq!(AudioQuality::Best.mp3_bitrate(), None);
        assert_eq!(AudioQuality::Mp3_320.mp3_bitrate(), Some("320K"));
        assert_eq!(AudioQuality::Mp3_192.mp3_bitrate(), Some("192K"));
        assert_eq!(VideoQuality::Best.max_height(), None);
        assert_eq!(VideoQuality::P1080.max_height(), Some(1080));
        assert_eq!(VideoQuality::P720.max_height(), Some(720));
        assert!(ThumbnailMode::Embed.embeds() && !ThumbnailMode::Embed.writes_file());
        assert!(ThumbnailMode::File.writes_file() && !ThumbnailMode::File.embeds());
        assert!(ThumbnailMode::Both.embeds() && ThumbnailMode::Both.writes_file());
        assert!(!ThumbnailMode::None.embeds() && !ThumbnailMode::None.writes_file());
        assert_eq!(YtdlpChannel::Stable.repo(), "yt-dlp/yt-dlp");
        assert_eq!(YtdlpChannel::Nightly.repo(), "yt-dlp/yt-dlp-nightly-builds");
    }

    #[test]
    fn every_setting_falls_back_to_its_default_on_a_fresh_db() {
        let db = db();
        assert_eq!(default_format(&db), Format::Audio);
        assert_eq!(audio_quality(&db), AudioQuality::Mp3_320);
        assert_eq!(video_quality(&db), VideoQuality::Best);
        assert_eq!(thumbnail_mode(&db), ThumbnailMode::Embed);
        assert_eq!(cookies(&db), CookiesMode::Session);
        assert_eq!(ytdlp_channel(&db), YtdlpChannel::Stable);
        assert_eq!(requested_quality(&db, Format::Audio), "mp3_320");
        assert_eq!(requested_quality(&db, Format::Video), "best");
    }

    #[test]
    fn stored_values_win_and_corrupt_ones_read_as_the_default() {
        let db = db();
        db.set_setting(DEFAULT_FORMAT_KEY, "video");
        db.set_setting(AUDIO_QUALITY_KEY, "best");
        db.set_setting(VIDEO_QUALITY_KEY, "720p");
        db.set_setting(THUMBNAIL_MODE_KEY, "none");
        db.set_setting(COOKIES_KEY, "none");
        db.set_setting(YTDLP_CHANNEL_KEY, "nightly");
        assert_eq!(default_format(&db), Format::Video);
        assert_eq!(audio_quality(&db), AudioQuality::Best);
        assert_eq!(video_quality(&db), VideoQuality::P720);
        assert_eq!(thumbnail_mode(&db), ThumbnailMode::None);
        assert_eq!(cookies(&db), CookiesMode::None);
        assert_eq!(ytdlp_channel(&db), YtdlpChannel::Nightly);

        db.set_setting(DEFAULT_FORMAT_KEY, "hologram");
        db.set_setting(AUDIO_QUALITY_KEY, "");
        db.set_setting(COOKIES_KEY, "chrome");
        db.set_setting(YTDLP_CHANNEL_KEY, "beta");
        assert_eq!(default_format(&db), Format::Audio);
        assert_eq!(audio_quality(&db), AudioQuality::Mp3_320);
        assert_eq!(cookies(&db), CookiesMode::Session);
        assert_eq!(ytdlp_channel(&db), YtdlpChannel::Stable);
    }

    #[test]
    fn the_folder_is_the_stored_absolute_path_or_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let default = data.join("downloads");
        assert_eq!(resolve_dir(None, default.clone()), default);
        assert_eq!(resolve_dir(Some("   "), default.clone()), default, "cleared = not set");
        assert_eq!(resolve_dir(Some("relative/dir"), default.clone()), default);
        let mine = tmp.path().join("mine");
        assert_eq!(resolve_dir(Some(&mine.to_string_lossy()), default.clone()), mine);

        let db = db();
        db.set_setting(DOWNLOADS_DIR_KEY, &mine.to_string_lossy());
        assert_eq!(downloads_dir(&db, &data), mine);
    }

    #[test]
    fn the_default_folder_is_music_or_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let music = tmp.path().join("Music");
        assert_eq!(
            default_dir_from(false, Some(music.clone()), &data),
            music.join("LiMusic Forge")
        );
        assert_eq!(default_dir_from(true, Some(music), &data), data.join("downloads"), "portable");
        assert_eq!(default_dir_from(false, None, &data), data.join("downloads"));
        assert_eq!(
            default_dir_from(false, Some(PathBuf::from("Music")), &data),
            data.join("downloads"),
            "a relative answer is no answer"
        );
    }
}
