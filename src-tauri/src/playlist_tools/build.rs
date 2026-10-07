//! Write a split's or a merge's result: new playlists (on the account or on this machine), or the
//! tracks appended to one you already have. Long on an account playlist, since every request waits
//! its turn on the shared pacer (`import::before_playlist_write`), so it reports progress as it goes
//! (`playlist-op-progress`) and can be stopped between requests. Whatever was made before a stop or
//! a failure is journaled all the same, so one undo takes it back.
//!
//! An extract (Tools ▸ Extract) is one list made of a playlist's filtered rows. As a move it also
//! takes those rows out of the source once they are in, and its undo puts them back where they
//! were before taking the new playlist away.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use innertube::SongItem;
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use super::journal::{self, Named, OpRecord, Restore, Step, Summary};
use super::rows::{self, handle};
use super::transfer::Mode;
use crate::commands::require_login;
use crate::db::now_secs;
use crate::import::{before_playlist_write, playlist_write_error};
use crate::state::{is_local_playlist, AppState, LOCAL_PLAYLIST_PREFIX};

/// One playlist to make.
#[derive(Debug, Clone, Deserialize)]
pub struct NewList {
    pub name: String,
    pub songs: Vec<SongItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "to", rename_all = "snake_case")]
pub enum Dest {
    /// New playlists, on this machine or on the account.
    New { local: bool },
    /// Every list appended to this playlist (a merge into one you have).
    Existing { id: String, title: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// The playlist being written now.
    pub current: String,
}

#[derive(Debug, Serialize)]
pub struct Built {
    /// The new playlists, as browse ids, with their names.
    pub created: Vec<Named>,
    pub added: usize,
    /// Rows a move took out of its source (an extract only).
    pub removed: usize,
    pub stopped: bool,
    pub error: Option<String>,
    pub op: Option<OpRecord>,
}

static CANCEL: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Stop the running build before its next request.
pub fn cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

fn emit(state: &AppState, p: Progress) {
    let _ = state.app.emit("playlist-op-progress", p);
}

/// The journal kind a build is recorded under: what the UI asked for, when it is one of these.
pub fn journal_kind(kind: &str) -> &'static str {
    match kind {
        "merge" => "merge",
        "extract" => "extract",
        _ => "split",
    }
}

/// Whether this build takes its rows out of the source afterwards: only an extract, as a move,
/// from exactly one playlist.
fn moves_out(kind: &str, mode: Mode, sources: &[Named]) -> bool {
    kind == "extract" && mode == Mode::Move && sources.len() == 1
}

pub async fn run(
    state: &Arc<AppState>,
    kind: &str,
    sources: Vec<Named>,
    lists: Vec<NewList>,
    dest: Dest,
    mode: Mode,
) -> Result<Built, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("Another playlist tool is still writing. Wait for it, or stop it.".into());
    }
    CANCEL.store(false, Ordering::Relaxed);
    let result = build(state, kind, sources, lists, dest, mode).await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn build(
    state: &Arc<AppState>,
    kind: &str,
    sources: Vec<Named>,
    lists: Vec<NewList>,
    dest: Dest,
    mode: Mode,
) -> Result<Built, String> {
    let cancelled = &|| CANCEL.load(Ordering::Relaxed);
    // A move reads the source as it is before anything is written, so a source that can't be read
    // fails the extract with nothing made, and the rows taken out later are the real ones.
    let source_rows = if moves_out(kind, mode, &sources) {
        Some(rows::read_all(state, &sources[0].id).await?)
    } else {
        None
    };
    let total: usize = lists.iter().map(|l| l.songs.len()).sum();
    let mut done = 0;
    let mut created: Vec<Named> = Vec::new();
    let mut added_rows: Vec<SongItem> = Vec::new();
    // Every song that went in, wherever: what a move takes out of the source.
    let mut went_in: Vec<SongItem> = Vec::new();
    let mut error = None;
    let mut first = true;

    for list in &lists {
        if cancelled() {
            break;
        }
        let (id, title) = match &dest {
            Dest::Existing { id, title } => (id.clone(), title.clone()),
            Dest::New { local } => {
                emit(state, Progress { done, total, current: list.name.clone() });
                let made = create(state, &list.name, *local, first, cancelled).await;
                first = first && *local; // an account create used the operation's first slot
                match made {
                    Ok(id) => {
                        created.push(Named { id: id.clone(), title: list.name.clone() });
                        (id, list.name.clone())
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
        };
        emit(state, Progress { done, total, current: title.clone() });
        // The add is a second write right behind the create, so on the account it waits its turn
        // too; `add_rows` only paces its own requests after the first.
        if !first && !is_local_playlist(&id) {
            if let Err(e) = before_playlist_write(state, cancelled).await {
                error = (e != "gone").then_some(e);
                break;
            }
        }
        first = false;
        // Duplicates are left out: a split never repeats a row, a merge deduped (or chose not to).
        match rows::add_rows(state, &id, &list.songs, false, cancelled).await {
            Ok(a) => {
                done += list.songs.len();
                went_in.extend(a.rows.iter().cloned());
                if matches!(dest, Dest::Existing { .. }) {
                    added_rows.extend(a.rows.into_iter().filter(|r| handle(r).is_some()));
                }
            }
            Err(e) => {
                error = (e != "gone").then_some(e);
                break;
            }
        }
        emit(state, Progress { done, total, current: title });
    }

    // A move takes out of the source what went in, even after a stop or a failure part way: those
    // rows are in the new place already, and the journal below puts them back on an undo.
    let mut taken: Vec<Restore> = Vec::new();
    if let Some(source_rows) = &source_rows {
        let wanted: Vec<SongItem> = lists.iter().flat_map(|l| l.songs.iter().cloned()).collect();
        let out = extract_removals(source_rows, &wanted, &went_in);
        let source = &sources[0].id;
        let mut ok = !out.is_empty();
        // Behind the writes before it, an account removal waits its turn like any of them.
        if ok && !is_local_playlist(source) && !went_in.is_empty() {
            if let Err(e) = before_playlist_write(state, &|| false).await {
                if error.is_none() {
                    error = (e != "gone").then_some(e);
                }
                ok = false;
            }
        }
        if ok {
            let gone: Vec<SongItem> = out.iter().map(|r| r.song.clone()).collect();
            match rows::remove_rows(state, source, &gone, &|| false).await {
                Ok(()) => taken = out,
                Err(e) => {
                    if error.is_none() {
                        error = (e != "gone").then_some(e);
                    }
                }
            }
        }
    }

    let source_id = sources.first().map(|s| s.id.clone());
    let undo = undo_steps(&created, &dest, &added_rows, source_id.as_deref(), &taken);
    let mut touched: Vec<String> = created.iter().map(|p| p.id.clone()).collect();
    if let Dest::Existing { id, .. } = &dest {
        touched.push(id.clone());
    }
    if let (Some(id), false) = (&source_id, taken.is_empty()) {
        touched.push(id.clone());
    }
    journal::announce(state, &touched);
    let mut playlists = sources;
    playlists.extend(created.iter().cloned());
    if let Dest::Existing { id, title } = &dest {
        playlists.push(Named { id: id.clone(), title: title.clone() });
    }
    let added = if matches!(dest, Dest::Existing { .. }) { added_rows.len() } else { done };
    let op = if undo.is_empty() {
        None
    } else {
        journal::record(state, kind, &Summary { playlists, count: added }, &undo)
    };
    Ok(Built {
        created,
        added,
        removed: taken.len(),
        stopped: CANCEL.load(Ordering::Relaxed),
        error,
        op,
    })
}

/// The source rows an extract-as-move takes out: one per song that went in, matched by videoId and
/// counted (as a transfer does), so a song refused or already in the target stays in the source.
/// When the extracted songs carry their source rows' handles, only those rows are candidates, so a
/// repeat the filter left out is never the one taken. Each comes with the handle of the first row
/// after it that stays, where the undo puts it back (`None`: the end). A row without a handle is
/// never taken.
pub fn extract_removals(
    source: &[SongItem],
    wanted: &[SongItem],
    went_in: &[SongItem],
) -> Vec<Restore> {
    let picked: HashSet<&str> = wanted.iter().filter_map(handle).collect();
    let mut quota: HashMap<&str, usize> = HashMap::new();
    for s in went_in {
        *quota.entry(s.video_id.as_str()).or_default() += 1;
    }
    let mut out_handles: HashSet<&str> = HashSet::new();
    for row in source {
        let Some(h) = handle(row) else { continue };
        if !picked.is_empty() && !picked.contains(h) {
            continue;
        }
        let Some(n) = quota.get_mut(row.video_id.as_str()) else { continue };
        if *n == 0 {
            continue;
        }
        *n -= 1;
        out_handles.insert(h);
    }
    // Walk back from the end so each taken row knows the next row that stays.
    let mut next: Option<String> = None;
    let mut out: Vec<Restore> = Vec::new();
    for row in source.iter().rev() {
        match handle(row) {
            Some(h) if out_handles.contains(h) => {
                out.push(Restore { song: row.clone(), before: next.clone() });
            }
            Some(h) => next = Some(h.to_owned()),
            None => {}
        }
    }
    out.reverse();
    out
}

/// What one undo of a build runs, in order: the source's rows back where they were first (their
/// anchors are still there to aim at), then each playlist made taken away, then the rows appended
/// to an existing target taken out.
pub fn undo_steps(
    created: &[Named],
    dest: &Dest,
    added_rows: &[SongItem],
    source: Option<&str>,
    taken: &[Restore],
) -> Vec<Step> {
    let mut undo = Vec::new();
    if let (Some(source), false) = (source, taken.is_empty()) {
        undo.push(journal::restore_step(source, taken));
    }
    undo.extend(created.iter().map(|p| Step::DeletePlaylist { playlist_id: p.id.clone() }));
    if let (Dest::Existing { id, .. }, false) = (dest, added_rows.is_empty()) {
        undo.push(Step::Remove { playlist_id: id.clone(), rows: added_rows.to_vec() });
    }
    undo
}

/// Make one empty playlist and answer its browse id. On the account it waits its turn like any
/// write (the first request of an operation goes straight out).
async fn create(
    state: &Arc<AppState>,
    name: &str,
    local: bool,
    first: bool,
    cancelled: rows::Cancelled<'_>,
) -> Result<String, String> {
    if local {
        let id = state.db.create_local_playlist(name, now_secs()).map_err(|e| e.to_string())?;
        return Ok(format!("{LOCAL_PLAYLIST_PREFIX}{id}"));
    }
    let client = require_login(state)?;
    if !first {
        before_playlist_write(state, cancelled).await?;
    }
    let id =
        state.it.create_playlist(client, name).await.map_err(|e| playlist_write_error(state, e))?;
    Ok(format!("VL{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &str, set: &str) -> SongItem {
        SongItem { video_id: v.into(), set_video_id: Some(set.into()), ..Default::default() }
    }
    fn song(v: &str) -> SongItem {
        SongItem { video_id: v.into(), ..Default::default() }
    }
    fn taken(out: &[Restore]) -> Vec<(&str, Option<&str>)> {
        out.iter().map(|r| (handle(&r.song).unwrap(), r.before.as_deref())).collect()
    }
    fn named(id: &str) -> Named {
        Named { id: id.into(), title: id.into() }
    }

    #[test]
    fn extract_is_its_own_kind_and_only_a_move_from_one_source_takes_rows_out() {
        assert_eq!(journal_kind("extract"), "extract");
        assert_eq!(journal_kind("merge"), "merge");
        assert_eq!(journal_kind("anything else"), "split");
        let one = [named("PLsrc")];
        assert!(moves_out("extract", Mode::Move, &one));
        assert!(!moves_out("extract", Mode::Copy, &one), "a copy leaves the source alone");
        assert!(!moves_out("merge", Mode::Move, &one), "only an extract moves");
        assert!(!moves_out("extract", Mode::Move, &[named("a"), named("b")]));
    }

    #[test]
    fn a_move_takes_out_what_went_in_with_anchors_for_the_undo() {
        let source = [row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4"), row("e", "5")];
        // b, d and e were extracted; d was refused by the target, so it never went in.
        let wanted = [row("b", "2"), row("d", "4"), row("e", "5")];
        let out = extract_removals(&source, &wanted, &[song("b"), song("e")]);
        assert_eq!(taken(&out), [("2", Some("3")), ("5", None)], "d stays; e goes back last");
    }

    #[test]
    fn the_anchor_skips_rows_taken_with_it() {
        let source = [row("a", "1"), row("b", "2"), row("c", "3"), row("d", "4")];
        let wanted = [row("b", "2"), row("c", "3")];
        let out = extract_removals(&source, &wanted, &[song("b"), song("c")]);
        assert_eq!(taken(&out), [("2", Some("4")), ("3", Some("4"))]);
    }

    #[test]
    fn only_the_extracted_copy_of_a_repeat_goes() {
        // The same song twice; the filter picked the second copy (row 3) only.
        let source = [row("a", "1"), row("b", "2"), row("a", "3")];
        let out = extract_removals(&source, &[row("a", "3")], &[song("a")]);
        assert_eq!(taken(&out), [("3", None)]);
        // Without handles to go by, it's matched by videoId and counted: one copy, the first.
        let out = extract_removals(&source, &[song("a")], &[song("a")]);
        assert_eq!(taken(&out), [("1", Some("2"))]);
    }

    #[test]
    fn a_row_without_a_handle_is_never_taken() {
        let source = [song("a"), row("b", "2")];
        let out = extract_removals(&source, &[song("a"), song("b")], &[song("a"), song("b")]);
        assert_eq!(taken(&out), [("2", None)]);
    }

    #[test]
    fn the_undo_of_a_move_into_a_new_playlist_restores_the_source_then_deletes_it() {
        let source = [row("a", "1"), row("b", "2"), row("c", "3")];
        let out = extract_removals(&source, &[row("b", "2")], &[song("b")]);
        let made = [named("VLnew")];
        let steps = undo_steps(&made, &Dest::New { local: false }, &[], Some("VLsrc"), &out);
        assert_eq!(
            steps,
            vec![
                Step::Restore {
                    playlist_id: "VLsrc".into(),
                    // The old handle is dropped: the row that comes back gets a new one.
                    rows: vec![Restore { song: song("b"), before: Some("3".into()) }],
                },
                Step::DeletePlaylist { playlist_id: "VLnew".into() },
            ]
        );
        // A copy: the new playlist is all there is to take back.
        let steps = undo_steps(&made, &Dest::New { local: true }, &[], Some("VLsrc"), &[]);
        assert_eq!(steps, vec![Step::DeletePlaylist { playlist_id: "VLnew".into() }]);
    }

    #[test]
    fn the_undo_of_a_move_into_an_existing_playlist_takes_the_new_rows_out_last() {
        let source = [row("a", "1"), row("b", "2")];
        let out = extract_removals(&source, &[row("a", "1")], &[song("a")]);
        let dest = Dest::Existing { id: "LOCALPLAYLIST:7".into(), title: "T".into() };
        let added = [row("a", "99")];
        let steps = undo_steps(&[], &dest, &added, Some("LOCALPLAYLIST:3"), &out);
        let back = vec![Restore { song: song("a"), before: Some("2".into()) }];
        assert_eq!(
            steps,
            vec![
                Step::Restore { playlist_id: "LOCALPLAYLIST:3".into(), rows: back },
                Step::Remove { playlist_id: "LOCALPLAYLIST:7".into(), rows: added.to_vec() },
            ]
        );
        // Nothing went in, nothing was taken: nothing to undo, so nothing is journaled.
        assert!(undo_steps(&[], &dest, &[], Some("LOCALPLAYLIST:3"), &[]).is_empty());
    }
}
