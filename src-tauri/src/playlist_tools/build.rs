//! Write a split's or a merge's result: new playlists (on the account or on this machine), or the
//! tracks appended to one you already have. Long on an account playlist, since every request waits
//! its turn on the shared pacer (`import::before_playlist_write`), so it reports progress as it goes
//! (`playlist-op-progress`) and can be stopped between requests. Whatever was made before a stop or
//! a failure is journaled all the same, so one undo takes it back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use innertube::SongItem;
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use super::journal::{self, Named, OpRecord, Step, Summary};
use super::rows::{self, handle};
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

pub async fn run(
    state: &Arc<AppState>,
    kind: &str,
    sources: Vec<Named>,
    lists: Vec<NewList>,
    dest: Dest,
) -> Result<Built, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("Another playlist tool is still writing. Wait for it, or stop it.".into());
    }
    CANCEL.store(false, Ordering::Relaxed);
    let result = build(state, kind, sources, lists, dest).await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn build(
    state: &Arc<AppState>,
    kind: &str,
    sources: Vec<Named>,
    lists: Vec<NewList>,
    dest: Dest,
) -> Result<Built, String> {
    let cancelled = &|| CANCEL.load(Ordering::Relaxed);
    let total: usize = lists.iter().map(|l| l.songs.len()).sum();
    let mut done = 0;
    let mut created: Vec<Named> = Vec::new();
    let mut added_rows: Vec<SongItem> = Vec::new();
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

    let mut undo: Vec<Step> =
        created.iter().map(|p| Step::DeletePlaylist { playlist_id: p.id.clone() }).collect();
    let mut touched: Vec<String> = created.iter().map(|p| p.id.clone()).collect();
    if let Dest::Existing { id, .. } = &dest {
        touched.push(id.clone());
        if !added_rows.is_empty() {
            undo.push(Step::Remove { playlist_id: id.clone(), rows: added_rows.clone() });
        }
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
    Ok(Built { created, added, stopped: CANCEL.load(Ordering::Relaxed), error, op })
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
