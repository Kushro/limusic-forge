//! Downloads with yt-dlp, ported from PlaylistForge's `download` module.
//!
//! - [`settings`]: the `downloads.*` defaults (folder, format, qualities, cover, cookies, channel).
//! - [`ytdlp_args`]: the argument vector and the output parsing, pure.
//! - [`tools`]: yt-dlp and ffmpeg, installed into `<data>/bin` with checksums on Windows, from
//!   PATH elsewhere.
//! - [`cookies`]: the session's cookies as a per-run Netscape file.
//! - [`verify`]: whether a downloaded file is still there, or moved.
//! - [`runner`]: the one task that runs the queue.
//!
//! The queue itself is the `downloads` table (`db.rs`).

pub mod cookies;
pub mod runner;
pub mod settings;
pub mod tools;
pub mod verify;
pub mod ytdlp_args;
