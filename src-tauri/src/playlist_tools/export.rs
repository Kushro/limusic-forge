//! A playlist out to a file. PlaylistForge wrote CSV in its core and never wired it to a button;
//! here it is one menu item away, in three formats:
//!
//! - **CSV** in Exportify's column names, which is what this app's own import reads
//!   (`spotify::read_file`), so an export brings a playlist to another install, or back after a
//!   delete, through Import.
//! - **JSON**: every field the app knows about each track, for scripts.
//! - **M3U8**: a playlist any player opens, one `music.youtube.com` link per track.

use innertube::SongItem;
use serde::Deserialize;

use super::dedup::duration_secs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Csv,
    Json,
    M3u8,
}

fn watch_url(s: &SongItem) -> String {
    format!("https://music.youtube.com/watch?v={}", s.video_id)
}

/// The artist line as a list: the names it links to when it links any (the runs between them are
/// the separators), else the line itself.
fn artist_names(s: &SongItem) -> Vec<String> {
    let linked: Vec<String> =
        s.artist_runs.iter().filter(|r| r.id.is_some()).map(|r| r.text.trim().to_owned()).collect();
    if linked.is_empty() {
        vec![s.artists.clone()]
    } else {
        linked
    }
}

pub fn render(format: Format, title: &str, rows: &[SongItem]) -> Result<String, String> {
    match format {
        Format::Csv => csv_text(title, rows),
        Format::Json => {
            let doc = serde_json::json!({ "title": title, "tracks": rows });
            serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())
        }
        Format::M3u8 => Ok(m3u8(title, rows)),
    }
}

fn csv_text(title: &str, rows: &[SongItem]) -> Result<String, String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    let header = [
        "Track Name",
        "Artist Name(s)",
        "Album Name",
        "Duration (ms)",
        "Explicit",
        "Playlist Name",
        "YouTube Video ID",
        "YouTube URL",
    ];
    w.write_record(header).map_err(|e| e.to_string())?;
    for s in rows {
        let ms = duration_secs(s.duration.as_deref()).map(|d| (d * 1000).to_string());
        w.write_record([
            s.title.as_str(),
            &artist_names(s).join(";"),
            s.album.as_deref().unwrap_or(""),
            ms.as_deref().unwrap_or(""),
            if s.explicit { "true" } else { "false" },
            title,
            &s.video_id,
            &watch_url(s),
        ])
        .map_err(|e| e.to_string())?;
    }
    let bytes = w.into_inner().map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn m3u8(title: &str, rows: &[SongItem]) -> String {
    let mut out = format!("#EXTM3U\n#PLAYLIST:{}\n", one_line(title));
    for s in rows {
        let secs = duration_secs(s.duration.as_deref()).unwrap_or(-1);
        out.push_str(&format!(
            "#EXTINF:{secs},{} - {}\n{}\n",
            one_line(&s.artists),
            one_line(&s.title),
            watch_url(s)
        ));
    }
    out
}

/// M3U is line based: a title with a newline in it would end its own entry.
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(v: &str, title: &str, artists: &str, dur: &str) -> SongItem {
        SongItem {
            video_id: v.into(),
            title: title.into(),
            artists: artists.into(),
            duration: Some(dur.into()),
            album: Some("Album".into()),
            ..Default::default()
        }
    }

    #[test]
    fn csv_comes_back_in_through_the_import() {
        let rows = vec![
            song("abc", "Hello, \"World\"", "Alpha", "3:45"),
            song("def", "Second", "Beta", "1:00:00"),
        ];
        let text = render(Format::Csv, "My mix", &rows).unwrap();
        let lib = crate::spotify::read_file(text.as_bytes(), "x.csv").unwrap();
        assert_eq!(lib.lists.len(), 1);
        let list = &lib.lists[0];
        assert_eq!(list.name, "My mix");
        assert_eq!(list.tracks[0].title, "Hello, \"World\"");
        assert_eq!(list.tracks[0].artists, ["Alpha"]);
        assert_eq!(list.tracks[0].duration_ms, Some(225_000));
        assert_eq!(list.tracks[1].duration_ms, Some(3_600_000));
    }

    #[test]
    fn m3u8_and_json() {
        let rows = vec![song("abc", "Line\nbreak", "Alpha", "3:45")];
        let m = render(Format::M3u8, "Mix", &rows).unwrap();
        assert_eq!(
            m,
            "#EXTM3U\n#PLAYLIST:Mix\n#EXTINF:225,Alpha - Line break\nhttps://music.youtube.com/watch?v=abc\n"
        );
        let j: serde_json::Value =
            serde_json::from_str(&render(Format::Json, "Mix", &rows).unwrap()).unwrap();
        assert_eq!(j["tracks"][0]["video_id"], "abc");
    }
}
