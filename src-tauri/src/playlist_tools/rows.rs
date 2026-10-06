//! Rows of one playlist, read and edited the same way whether it lives on the account or on this
//! machine. Every playlist tool goes through here, so the `LOCALPLAYLIST:` branch, the pacing and
//! the membership index are each handled once.
//!
//! A row is a `SongItem` whose `set_video_id` is its handle: YouTube's playlistSetVideoId for an
//! account playlist, the SQLite row id for a local one (`local_playlist_page` hands out the same).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use innertube::SongItem;

use super::lis::{self, Move};
use crate::commands::{db_err, editable_playlist, local_key, metadata_client, playlist_row};
use crate::db::now_secs;
use crate::import::{before_playlist_write, playlist_write_error, youtube_cooldown};
use crate::local::SONG_PREFIX;
use crate::state::{is_local_playlist, AppState};

/// Continuation pages read for a whole playlist: 100 rows a page, so YouTube's 5000-row cap.
const MAX_PAGES: usize = 50;
/// Actions per `edit_playlist` request. YouTube has taken a few hundred in one body (see
/// `playlist_remove_many`), so this is well inside it, and a refusal costs little to retry.
const ACTIONS_PER_REQUEST: usize = 100;

/// Whether this operation should stop. The bulk runner's cancel flag; `&|| false` for none.
pub type Cancelled<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// The handle of a row, which every edit addresses it by.
pub fn handle(row: &SongItem) -> Option<&str> {
    row.set_video_id.as_deref()
}

/// Every row of the playlist, in its order. An account playlist is read page by page. Any playlist
/// can be read (a split or a merge may start from Liked Music or someone else's list); rows of one
/// you can't edit carry no handle, and every edit below skips a row without one.
pub async fn read_all(state: &Arc<AppState>, playlist_id: &str) -> Result<Vec<SongItem>, String> {
    if is_local_playlist(playlist_id) {
        let key = local_key(playlist_id)?;
        return Ok(state
            .db
            .local_playlist_tracks(key)
            .into_iter()
            .filter_map(|(row, json)| {
                let song: SongItem = serde_json::from_str(&json).ok()?;
                Some(SongItem { set_video_id: Some(row.to_string()), ..song })
            })
            .collect());
    }
    let client = metadata_client(state)?;
    let page = state.it.playlist(client, playlist_id, None).await.map_err(|e| e.to_string())?;
    let mut out = page.items;
    let mut token = page.continuation;
    for _ in 0..MAX_PAGES {
        let Some(next) = token.take() else { break };
        let more =
            state.it.playlist_continuation(client, &next).await.map_err(|e| e.to_string())?;
        out.extend(more.items);
        token = more.continuation;
    }
    Ok(out)
}

/// One request slot. The first request of an operation goes straight out (a single drag or drop
/// is not made to wait), and only refuses during a cooldown; every request after it is paced.
async fn slot(state: &AppState, first: bool, cancelled: Cancelled<'_>) -> Result<(), String> {
    if first {
        if let Some(until) = youtube_cooldown(state) {
            return Err(format!("cooldown:{until}"));
        }
        return Ok(());
    }
    before_playlist_write(state, cancelled).await
}

/// Apply `moves` in order (see `lis::reorder_moves`).
pub async fn move_rows(
    state: &Arc<AppState>,
    playlist_id: &str,
    moves: &[Move],
    cancelled: Cancelled<'_>,
) -> Result<(), String> {
    if moves.is_empty() {
        return Ok(());
    }
    if is_local_playlist(playlist_id) {
        let key = local_key(playlist_id)?;
        let mut order: Vec<String> =
            state.db.local_playlist_tracks(key).into_iter().map(|(id, _)| id.to_string()).collect();
        lis::apply_moves(&mut order, moves);
        let ids: Vec<i64> = order.iter().filter_map(|id| id.parse().ok()).collect();
        return state.db.reorder_local_playlist(key, &ids, now_secs()).map_err(db_err);
    }
    let client = editable_playlist(state, playlist_id)?;
    for (i, chunk) in moves.chunks(ACTIONS_PER_REQUEST).enumerate() {
        slot(state, i == 0, cancelled).await?;
        let pairs: Vec<(String, Option<String>)> =
            chunk.iter().map(|m| (m.row.clone(), m.before.clone())).collect();
        state
            .it
            .playlist_move_many(client, playlist_id, &pairs)
            .await
            .map_err(|e| playlist_write_error(state, e))?;
    }
    Ok(())
}

/// Put the playlist in `target` order (row handles), reading what it holds now first so the moves
/// are computed against the real list. Answers the order it was in before (for the undo journal)
/// and how many rows had to move.
pub async fn reorder(
    state: &Arc<AppState>,
    playlist_id: &str,
    target: &[String],
    cancelled: Cancelled<'_>,
) -> Result<(Vec<String>, usize), String> {
    let before: Vec<String> =
        read_all(state, playlist_id).await?.iter().filter_map(handle).map(str::to_owned).collect();
    let moves = lis::reorder_moves(&before, target);
    move_rows(state, playlist_id, &moves, cancelled).await?;
    Ok((before, moves.len()))
}

/// What an add did: the rows it created (with their handles), and the songs it left out.
#[derive(Debug, Default)]
pub struct Added {
    pub rows: Vec<SongItem>,
    /// Already in the playlist, and duplicates weren't allowed.
    pub duplicates: Vec<SongItem>,
    /// Refused for another reason: a file on this computer bound for YouTube, a track YouTube
    /// won't take (taken down, blocked in the region).
    pub refused: Vec<SongItem>,
}

/// Append `songs`. Without `allow_duplicates` a song the playlist already holds is skipped, the
/// way YouTube refuses one. Files on this computer only go into a local playlist.
pub async fn add_rows(
    state: &Arc<AppState>,
    playlist_id: &str,
    songs: &[SongItem],
    allow_duplicates: bool,
    cancelled: Cancelled<'_>,
) -> Result<Added, String> {
    if is_local_playlist(playlist_id) {
        // The table holds a video once per playlist, so duplicates are never possible here.
        let key = local_key(playlist_id)?;
        let rows = songs
            .iter()
            .map(|s| {
                let s = playlist_row(s.clone());
                serde_json::to_string(&s).map(|json| (s.video_id, json)).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let went_in = state.db.add_local_playlist_tracks(key, &rows, now_secs()).map_err(db_err)?;
        let mut added = Added::default();
        let mut new_ids: HashMap<String, String> = state
            .db
            .local_playlist_tracks(key)
            .into_iter()
            .filter_map(|(row, json)| {
                let s: SongItem = serde_json::from_str(&json).ok()?;
                Some((s.video_id, row.to_string()))
            })
            .collect();
        for (song, ok) in songs.iter().zip(went_in) {
            if ok {
                let set = new_ids.remove(&song.video_id);
                added.rows.push(SongItem { set_video_id: set, ..playlist_row(song.clone()) });
            } else {
                added.duplicates.push(song.clone());
            }
        }
        return Ok(added);
    }

    let client = editable_playlist(state, playlist_id)?;
    let mut added = Added::default();
    let (files, songs): (Vec<&SongItem>, Vec<&SongItem>) =
        songs.iter().partition(|s| s.video_id.starts_with(SONG_PREFIX));
    added.refused.extend(files.into_iter().cloned());
    let mut todo: VecDeque<Vec<&SongItem>> =
        songs.chunks(ACTIONS_PER_REQUEST).map(<[&SongItem]>::to_vec).collect();
    let mut first = true;
    // YouTube applies a batch whole or not at all, so a refused one is halved until the track it
    // won't take is alone (as the import's `add_all` does). Pushback stops at once.
    while let Some(piece) = todo.pop_front() {
        slot(state, first, cancelled).await?;
        first = false;
        let ids: Vec<String> = piece.iter().map(|s| s.video_id.clone()).collect();
        match state.it.playlist_add_many_rows(client, playlist_id, &ids, allow_duplicates).await {
            Ok(created) => {
                // Match the created rows back to the songs by videoId, in order, so a song added
                // twice in one batch gets both of its handles.
                let mut by_video: HashMap<String, VecDeque<String>> = HashMap::new();
                for (v, set) in created {
                    by_video.entry(v).or_default().push_back(set);
                }
                for song in piece {
                    let set = by_video.get_mut(&song.video_id).and_then(VecDeque::pop_front);
                    let row = playlist_row((*song).clone());
                    if let Ok(json) = serde_json::to_string(&row) {
                        state.db.put_playlist_song(playlist_id, &song.video_id, &json);
                    }
                    added.rows.push(SongItem { set_video_id: set, ..row });
                }
            }
            Err(e) if is_pushback(&e) => return Err(playlist_write_error(state, e)),
            Err(innertube::Error::AlreadyInPlaylist) if piece.len() == 1 => {
                state.db.add_playlist_track(playlist_id, &piece[0].video_id);
                added.duplicates.push(piece[0].clone());
            }
            Err(e) if piece.len() == 1 => {
                tracing::warn!(error = %e, video_id = %piece[0].video_id, "playlist tools: YouTube won't take this track");
                added.refused.push(piece[0].clone());
            }
            Err(_) => {
                let (a, b) = piece.split_at(piece.len() / 2);
                todo.push_front(b.to_vec());
                todo.push_front(a.to_vec());
            }
        }
    }
    Ok(added)
}

/// Errors that end the operation rather than the one batch: YouTube slowing us down, or the
/// session gone.
fn is_pushback(e: &innertube::Error) -> bool {
    matches!(e, innertube::Error::SessionExpired)
        || matches!(e, innertube::Error::Http(h) if matches!(h.status().map(|s| s.as_u16()), Some(429 | 403)))
}

/// Take `rows` out (by handle). All or nothing per request.
pub async fn remove_rows(
    state: &Arc<AppState>,
    playlist_id: &str,
    rows: &[SongItem],
    cancelled: Cancelled<'_>,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    if is_local_playlist(playlist_id) {
        let key = local_key(playlist_id)?;
        let ids: Vec<i64> = rows.iter().filter_map(|r| handle(r)?.parse().ok()).collect();
        return state.db.remove_local_playlist_tracks(key, &ids, now_secs()).map_err(db_err);
    }
    let client = editable_playlist(state, playlist_id)?;
    for (i, chunk) in rows.chunks(ACTIONS_PER_REQUEST).enumerate() {
        slot(state, i == 0, cancelled).await?;
        let tracks: Vec<(String, String)> = chunk
            .iter()
            .filter_map(|r| Some((r.video_id.clone(), handle(r)?.to_owned())))
            .collect();
        state
            .it
            .playlist_remove_many(client, playlist_id, &tracks)
            .await
            .map_err(|e| playlist_write_error(state, e))?;
        for (video_id, _) in &tracks {
            state.db.remove_playlist_track(playlist_id, video_id);
        }
    }
    Ok(())
}
