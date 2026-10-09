//! Local SQLite state. context/11 §state. `rusqlite` (bundled) behind a Mutex — one file, low
//! write volume, no async pool needed (plan decision).

use std::sync::Mutex;

use md5::{Digest, Md5};
use rusqlite::{Connection, OptionalExtension};

/// Clearing the lyrics cache spares songs whose source was picked by hand in the lyrics footer (or
/// their timing nudged): those are the user's choices, not a cache.
// ponytail: the pin lives in the lyrics JSON rather than a column, it is read nowhere else.
const CLEAR_LYRICS: &str =
    "DELETE FROM lyrics_cache WHERE lyrics IS NULL OR json_extract(lyrics, '$.pinned') IS NOT 1";

/// The connection, and why the schema migration failed on open, if it did (see
/// [`Db::migration_error`]).
pub struct Db(Mutex<Connection>, Option<String>);

/// The stored-account key (multi-account support): a stable per-Google-account identifier derived
/// from the long-lived `SAPISID` cookie value, the one piece of the jar Google does not rotate.
/// Versioned MD5 (the `md-5` crate is already here for the Last.fm signature), hex-encoded,
/// deliberately not `DefaultHasher`, whose output Rust does not guarantee to stay stable across
/// releases: a persisted key must survive toolchain upgrades. Not a security digest, just a stable
/// identifier, and the SAPISID itself never appears in the key the UI gets to see.
///
/// `None` for a jar with no SAPISID, which is not a signed-in session at all: every caller needs
/// to skip such a cookie rather than file it under a key shared with every other broken jar.
pub fn account_key(session_cookie: &str) -> Option<String> {
    let sapisid = innertube::cookie_sapisid(session_cookie)?;
    let mut digest = Md5::new();
    digest.update(b"limusic-google-account-v1");
    digest.update(sapisid.as_bytes());
    Some(format!("ga-{:x}", digest.finalize()))
}

/// One saved Google account. Canonical multi-account state; the `settings` rows `session_cookie`,
/// `selected_identity_json`, `data_sync_id`, `account_json` and `visitor_data` remain as
/// projections of the *active* account, so everything that read them before (startup bootstrap in
/// lib.rs, `account_snapshot`, the channel switcher, legacy fallbacks) keeps working unchanged.
#[derive(Debug, Clone)]
pub struct StoredAccount {
    pub id: String,
    pub session_cookie: String,
    pub data_sync_id: Option<String>,
    pub selected_identity_json: Option<String>,
    pub account_json: Option<String>,
    pub visitor_data: Option<String>,
    pub added_at: i64,
}

/// Unix seconds. Lives here because every wall-clock value in the app is a column in this file
/// (`expires_at`, `played_at`, `fetched_at`) or something stored alongside them.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One row of `import_matches`: a Spotify track and what it is on YouTube Music. `video_id` is
/// `None` for a track that wasn't found. The JSON columns are whole `SongItem`s.
pub struct ImportMatch {
    pub video_id: Option<String>,
    pub song_json: Option<String>,
    pub tier: String,
    pub candidates_json: Option<String>,
    pub manual: bool,
    pub updated_at: i64,
}

/// One row of `recover_titles`: what a dead video was called. `found = false` is a negative
/// verdict ("looked, no usable title"), with `title` normally `None`; [`Db::recover_title_get`]
/// stops returning it after [`RECOVER_NEGATIVE_TTL_SECS`]. `source` names where the title came
/// from (`"wayback"`, `"manual"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTitle {
    pub title: Option<String>,
    pub artists: Option<String>,
    pub source: String,
    pub found: bool,
    pub checked_at: i64,
}

/// How long a negative `recover_titles` row is believed: 30 days. Wayback keeps crawling, so
/// "nothing archived" is worth asking again eventually; a found title never goes stale.
pub const RECOVER_NEGATIVE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// A cached stream URL with its expiry. Never a source of truth — purely a latency cache.
pub struct CachedStream {
    pub url: String,
    pub itag: i64,
    pub expires_at: i64,
    /// Raw `loudnessDb` (main-client metadata) so a cache-hit replay still normalizes loudness.
    pub loudness_db: Option<f64>,
    /// YouTube's `musicVideoType` verdict, cached for the same reason: a hit skips `/player`, and
    /// without it a replay inside the cache window can't tell the player view whether the track
    /// has a music video. `None` on rows written before the column existed.
    pub is_video: Option<bool>,
    /// The watch-history ping (`videostatsPlaybackUrl` + the client that was issued it), cached for
    /// the same reason again: a hit skips `/player`, and without it every replay inside the cache
    /// window went unregistered. That included plenty of *first* listens, because the gapless
    /// lookahead caches the next track and a non-gapless advance then re-resolves it. Issue #83.
    pub ping_url: Option<String>,
    pub ping_client: Option<String>,
    /// The client whose URL this is ("WEB_REMIX", "VISIONOS", ...), so a replay can rebuild the
    /// headers the URL was validated under (the User-Agent is per client; the wrong one is a 403)
    /// and a failure toast can name it. `ping_client` is not a substitute: the orchestrator prefers
    /// the *main* client's tracking block even when a fallback won the stream.
    pub client: Option<String>,
}

impl Db {
    /// [`Db::open`], but a database that cannot be opened is moved aside and a fresh one is
    /// created in its place.
    ///
    /// The file holds a cache plus a little UI state (queue, settings, play counts), and the
    /// playlists kept on this machine. Losing it costs the user their resume position, their On
    /// Repeat history and those playlists (still in the moved-aside copy). Failing to open it costs
    /// them the whole app: `open` is called from Tauri's `setup`, before any window exists, so a
    /// hard failure there is a process that starts and vanishes with nothing on screen. Between
    /// those two, starting is worth more.
    ///
    /// The bad file is kept, never deleted, so a user who cares can be walked through recovering
    /// rows from it. The name carries a timestamp so a repeated failure does not overwrite the
    /// first (and most likely useful) copy.
    ///
    /// Only SQLite's own verdict that the file is damaged (`NotADatabase`, `DatabaseCorrupt`) moves
    /// it. Anything else (locked, permissions, a missing directory) says nothing about the data,
    /// and moving a healthy library aside for it would hand the user an empty one for nothing, so
    /// that error is returned as is.
    ///
    /// Returns the error from the *second* attempt if even a fresh file will not open, because at
    /// that point the problem is the directory or the disk, not the data.
    pub fn open_or_quarantine(
        path: &std::path::Path,
    ) -> rusqlite::Result<(Self, Option<std::path::PathBuf>)> {
        use rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
        match Self::open(path) {
            Ok(db) => Ok((db, None)),
            Err(first)
                if !matches!(first.sqlite_error_code(), Some(NotADatabase | DatabaseCorrupt)) =>
            {
                Err(first)
            }
            Err(first) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let aside = path.with_extension(format!("corrupt-{stamp}.sqlite"));
                tracing::error!(
                    "cannot open {}: {first}. Moving it to {}",
                    path.display(),
                    aside.display()
                );
                // WAL leaves -wal and -shm beside the database. Move them too, or the fresh file
                // inherits a journal that does not belong to it. Suffixed on the OsStr, not via
                // `display()`, which is lossy on a non-UTF-8 path and would rename nothing.
                for suffix in ["", "-wal", "-shm"] {
                    let with = |p: &std::path::Path| {
                        let mut s = p.as_os_str().to_owned();
                        s.push(suffix);
                        std::path::PathBuf::from(s)
                    };
                    let _ = std::fs::rename(with(path), with(&aside));
                }
                Self::open(path).map(|db| (db, Some(aside)))
            }
        }
    }

    pub fn open(path: &std::path::Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // This file is a cache plus a little UI state, and it is written on every volume nudge,
        // pause, track change and queue edit. WAL keeps those off a full rollback journal, and
        // `synchronous=NORMAL` drops the fsync per commit: a power cut can lose the last few
        // seconds of "what was playing", which is the correct trade for a music player, and no
        // crash of ours can corrupt the file either way. `journal_mode` answers with a row, so it
        // is a query rather than a `pragma_update`.
        let _ = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0));
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        // `downloads.video_id` references `videos`; SQLite only holds it to that when asked.
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS stream_url_cache (
                video_id    TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                itag        INTEGER NOT NULL,
                expires_at  INTEGER NOT NULL,
                loudness_db REAL,
                is_video    INTEGER,
                ping_url    TEXT,
                ping_client TEXT
            );
            CREATE TABLE IF NOT EXISTS lyrics_cache (
                video_id   TEXT PRIMARY KEY,
                lyrics     TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plays (
                id        INTEGER PRIMARY KEY,
                video_id  TEXT NOT NULL,
                played_at INTEGER NOT NULL,
                song_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS plays_played_at ON plays(played_at);
            CREATE TABLE IF NOT EXISTS local_tracks (
                path          TEXT PRIMARY KEY,
                title         TEXT NOT NULL,
                artist        TEXT NOT NULL,
                album         TEXT NOT NULL,
                album_key     TEXT NOT NULL,
                album_artist  TEXT,
                track_no      INTEGER NOT NULL,
                duration_secs INTEGER NOT NULL,
                cover         TEXT,
                mtime         INTEGER NOT NULL,
                disc_no       INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS local_tracks_album ON local_tracks(album_key);
            CREATE TABLE IF NOT EXISTS playlist_track (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                PRIMARY KEY (playlist_id, video_id)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS playlist_track_video ON playlist_track(video_id);
            CREATE TABLE IF NOT EXISTS accounts (
                id                     TEXT PRIMARY KEY,
                session_cookie         TEXT NOT NULL,
                data_sync_id           TEXT,
                selected_identity_json TEXT,
                account_json           TEXT,
                visitor_data           TEXT,
                added_at               INTEGER NOT NULL
            );
            -- Playlists kept on this machine, no account needed (issue #251). Unlike everything
            -- above except `accounts`, this is the user's own data and not a cache: nothing
            -- rebuilds it. AUTOINCREMENT on both so an id is never handed out twice: a shortcut,
            -- a pin or an open page left pointing at a deleted playlist (or a removed row) must
            -- not quietly land on whatever took its number.
            CREATE TABLE IF NOT EXISTS local_playlists (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                title       TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            -- One row per track, in the order added (`id`). The row id is the track's
            -- `set_video_id`, the same handle a YouTube playlist row carries for its removal.
            -- `song_json` is the whole `SongItem`, so the playlist opens with no network at all.
            CREATE TABLE IF NOT EXISTS local_playlist_tracks (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                playlist_id INTEGER NOT NULL,
                video_id    TEXT NOT NULL,
                song_json   TEXT NOT NULL,
                added_at    INTEGER NOT NULL,
                position    INTEGER,
                UNIQUE (playlist_id, video_id)
            );
            CREATE INDEX IF NOT EXISTS local_playlist_tracks_video
                ON local_playlist_tracks(video_id);
            -- The playlist tools' journal (dedupe, move, reorder, split, merge): one row per
            -- operation with the steps that undo it. `inverse_json` is opaque here;
            -- `playlist_tools::journal` writes and reads it. Pruned to the last 20 on every write.
            CREATE TABLE IF NOT EXISTS playlist_ops (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                kind         TEXT NOT NULL,
                account      TEXT,
                summary_json TEXT NOT NULL,
                inverse_json TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                undone_at    INTEGER
            );
            -- `playlist_alert` and the monitor's other tables are created by `migrate_v3`.
            -- Spotify import (#375): what each Spotify track turned out to be on YouTube Music,
            -- keyed by Spotify's track id. A cache, apart from the `manual` rows: those are the
            -- user's own picks, which every later import and "Update from Spotify" reuses.
            -- `candidates_json` is kept only for rows that still need a look.
            CREATE TABLE IF NOT EXISTS import_matches (
                key             TEXT PRIMARY KEY,
                video_id        TEXT,
                song_json       TEXT,
                tier            TEXT NOT NULL,
                candidates_json TEXT,
                manual          INTEGER NOT NULL DEFAULT 0,
                updated_at      INTEGER NOT NULL
            );
            -- The recovery assistant's title lookups (F4): what a dead video was called, from the
            -- Wayback Machine or another source, keyed by videoId. A cache: `found = 0` rows are
            -- "looked, nothing there" and are ignored once older than 30 days, so a later snapshot
            -- gets its chance. `artists` is free text, as the source gave it.
            CREATE TABLE IF NOT EXISTS recover_titles (
                video_id   TEXT PRIMARY KEY,
                title      TEXT,
                artists    TEXT,
                source     TEXT NOT NULL,
                found      INTEGER NOT NULL,
                checked_at INTEGER NOT NULL
            );
            "#,
        )?;
        // Migrate pre-Phase-4 DBs that predate the loudness_db column. Errors ("duplicate column")
        // on fresh DBs are expected and ignored — the cache is disposable anyway.
        let _ = conn.execute("ALTER TABLE stream_url_cache ADD COLUMN loudness_db REAL", []);
        // Same for the AlbumArtist tag, which older scans read but never stored. The SCAN_VERSION
        // bump in local.rs is what refills it; this only makes the column exist.
        let _ = conn.execute("ALTER TABLE local_tracks ADD COLUMN album_artist TEXT", []);
        // And the disc number (issue #315), refilled the same way.
        let _ = conn
            .execute("ALTER TABLE local_tracks ADD COLUMN disc_no INTEGER NOT NULL DEFAULT 0", []);
        // Same one-shot for the music-video verdict, except the rows that predate it have to go:
        // a cache hit skips `/player`, so a NULL there reads as "no music video" for as long as
        // the URL lives (hours). `execute` succeeds only on the launch that adds the column, so
        // this wipes the stale rows once. The cache is disposable; the next play refills it.
        if conn.execute("ALTER TABLE stream_url_cache ADD COLUMN is_video INTEGER", []).is_ok() {
            let _ = conn.execute("DELETE FROM stream_url_cache", []);
        }
        // The watch-history ping, added for the same reason (issue #83). No wipe: a NULL here just
        // means that one replay goes unregistered, which is exactly what every row did before.
        let _ = conn.execute("ALTER TABLE stream_url_cache ADD COLUMN ping_url TEXT", []);
        let _ = conn.execute("ALTER TABLE stream_url_cache ADD COLUMN ping_client TEXT", []);
        // The resolving client, so a replay can rebuild its headers and name itself. Rows that
        // predate it have neither, which is the bug, so wipe them once like `is_video` above.
        if conn.execute("ALTER TABLE stream_url_cache ADD COLUMN client TEXT", []).is_ok() {
            let _ = conn.execute("DELETE FROM stream_url_cache", []);
        }
        // Local files are no longer recorded as plays (see `AppState::on_position`), but 0.3.1
        // recorded them for a while, so clear out anything already sitting in On Repeat's table.
        let _ = conn.execute("DELETE FROM plays WHERE video_id LIKE 'LOCAL:%'", []);
        // Sweep dead stream URLs here as well as on write. `put_stream` only runs on a cache miss,
        // so a session spent replaying cached tracks never triggers one, and the backlog that
        // built up before anything pruned at all (1803 rows, 1772 of them expired, on a real
        // install) would sit there until it happened to.
        let _ = conn.execute("DELETE FROM stream_url_cache WHERE expires_at <= ?1", [now_secs()]);
        // #329 put LRCLIB's search ahead of Netease, QQ and Kugou, but cached hits never expire, so
        // every track they already answered would keep their lyrics. Drop those once; the next
        // play asks the chain again. `user_version` is the once-marker, unused before this.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if version < 1 {
            let _ = conn.execute(
                "DELETE FROM lyrics_cache WHERE json_extract(lyrics, '$.source')
                    IN ('Netease Cloud Music', 'QQ Music', 'Kugou')",
                [],
            );
            let _ = conn.execute_batch("PRAGMA user_version = 1");
        }
        // v1.0.0: the Boidu switch became one entry in an ordered provider list, and word-timed
        // providers joined the head of it. Boidu turned off carries over as `-boidu`. Cached hits
        // never expire, so without a purge no song already played would ever reach the new ones.
        if version < 2 {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO settings(key, value) SELECT 'lyrics_providers', '-boidu'
                    FROM settings WHERE key = 'lyrics_boidu' AND value = 'false'",
                [],
            );
            let _ = conn.execute("DELETE FROM settings WHERE key = 'lyrics_boidu'", []);
            let _ = conn.execute("DELETE FROM lyrics_cache", []);
            let _ = conn.execute_batch("PRAGMA user_version = 2");
        }
        // The index keeps each track's metadata too (the "in your playlists" view and the
        // monitor read it). NULL until the next sync fills it in.
        let _ = conn.execute("ALTER TABLE playlist_track ADD COLUMN song_json TEXT", []);
        // Local playlists became reorderable: rows sort by `position`, which older files lack.
        // The column is added once; the backfill runs every open, since a row an older build
        // appended after a downgrade has no position either. `id` is the order they had.
        let _ = conn.execute("ALTER TABLE local_playlist_tracks ADD COLUMN position INTEGER", []);
        let _ = conn
            .execute("UPDATE local_playlist_tracks SET position = id WHERE position IS NULL", []);
        // v3: the monitor's history (snapshots, runs, per-playlist sync), alerts with an id, a
        // dedupe key and a seen flag, `first_seen`, and the download queue. After the
        // `song_json` ALTER above, which the rebuilt index rows rely on. `user_version` alone is
        // not trusted: upstream LiMusic numbers its own schema with it, so a file it raised to 3
        // (or past it) on its own and then copied over would skip the migration. The artefact
        // check catches that; `migrate_v3` is idempotent and never lowers the version.
        let mut migration_error = None;
        if version < 3 || !v3_complete(&conn) {
            if let Err(e) = migrate_v3(&conn) {
                tracing::error!("schema v3 migration failed, rolled back: {e}");
                migration_error = Some(format!("schema v3 migration failed: {e}"));
            }
        }
        // v4: the Data API's accounts, the job queue, the quota ledger and the runner lock, plus
        // each indexed track's date added and each synced playlist's privacy. Same rules as v3
        // (artefact check, one transaction, never lowers the number), and only on top of a
        // complete v3: `playlist_sync`, which v4 extends, is a v3 table.
        if migration_error.is_none() && (version < 4 || !v4_complete(&conn)) {
            if let Err(e) = migrate_v4(&conn) {
                tracing::error!("schema v4 migration failed, rolled back: {e}");
                migration_error = Some(format!("schema v4 migration failed: {e}"));
            }
        }
        // The lookup behind `AlertRow::resolved` (a `restored` of the same playlist and track,
        // filed later). Only an index, so no version bump; and only once v3 has built the table.
        if has_table(&conn, "playlist_alert") {
            let _ = conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS playlist_alert_pvk \
                 ON playlist_alert(playlist_id, video_id, kind, at)",
            );
        }
        // One-time migration of the pre-multi-account single session into `accounts`. The legacy
        // settings rows stay in place as projections of the active account (see `StoredAccount`).
        let legacy_cookie = conn
            .query_row("SELECT value FROM settings WHERE key = 'session_cookie'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok();
        if let (Some(cookie), Some(id)) =
            (legacy_cookie.as_deref(), legacy_cookie.as_deref().and_then(account_key))
        {
            let existing: i64 =
                conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0)).unwrap_or(0);
            if existing == 0 {
                let get = |key: &str| -> Option<String> {
                    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
                        .ok()
                };
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO accounts(id, session_cookie, data_sync_id, \
                     selected_identity_json, account_json, visitor_data, added_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        id,
                        cookie,
                        get("data_sync_id"),
                        get("selected_identity_json"),
                        get("account_json"),
                        get("visitor_data"),
                        now_secs()
                    ],
                );
                let _ = conn.execute(
                    "INSERT INTO settings(key, value) VALUES('active_account', ?1) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [id],
                );
            }
        }
        // An account's id is derived from its own cookie, so any change to `account_key` (the
        // pre-release SHA-1 scheme was one) strands rows under keys the app no longer computes and
        // the same Google account turns up twice in the menu. Recompute every row's key from its
        // own cookie and fold the row onto it. Idempotent: once the ids agree this is one scan.
        // The statement must be dropped before the writes (rusqlite borrows the connection).
        let rows: Vec<(String, String, i64)> = {
            let mut found = Vec::new();
            if let Ok(mut stmt) = conn.prepare("SELECT id, session_cookie, added_at FROM accounts")
            {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
                }) {
                    found.extend(rows.flatten());
                }
            }
            found
        };
        for (old_id, cookie, added_at) in rows {
            let Some(new_id) = account_key(&cookie) else { continue };
            if new_id == old_id {
                continue;
            }
            // The row's new key and the `active_account` pointer at it have to land together: a
            // crash between them leaves the pointer at an id nothing computes any more, and the
            // next launch sees the ids already agreeing and re-scans nothing.
            // Every statement goes through `tx`, and the commit only happens if all of them
            // succeeded: dropping the transaction rolls the row back, which beats committing a
            // pointer to an id whose row never moved.
            let Ok(tx) = conn.unchecked_transaction() else { continue };
            let taken: i64 = tx
                .query_row("SELECT COUNT(*) FROM accounts WHERE id = ?1", [&new_id], |r| r.get(0))
                .unwrap_or(0);
            let moved = if taken == 0 {
                tx.execute("UPDATE accounts SET id = ?1 WHERE id = ?2", [&new_id, &old_id]).is_ok()
            } else {
                // The canonical row is already there, written by the current build, so its cookie
                // is the fresher one. Keep it, but inherit the older `added_at` so the menu order
                // does not jump, and drop the stale copy.
                tx.execute(
                    "UPDATE accounts SET added_at = MIN(added_at, ?1) WHERE id = ?2",
                    rusqlite::params![added_at, &new_id],
                )
                .is_ok()
                    && tx.execute("DELETE FROM accounts WHERE id = ?1", [&old_id]).is_ok()
            };
            let pointed = tx
                .execute(
                    "UPDATE settings SET value = ?1 WHERE key = 'active_account' AND value = ?2",
                    [&new_id, &old_id],
                )
                .is_ok();
            if moved && pointed {
                let _ = tx.commit();
            }
        }
        Ok(Db(Mutex::new(conn), migration_error))
    }

    /// Why the schema migration failed when this file was opened, or `None` if it did not (or had
    /// nothing to do). The file still opens at its old schema; the next launch tries again.
    pub fn migration_error(&self) -> Option<String> {
        self.1.clone()
    }

    /// The connection itself, for the modules that keep their own SQL next to their types
    /// (`quota`, `jobs`, `ytdata_accounts`). A poisoned lock is taken over rather than
    /// propagated: every write here is a transaction or a single statement, so a panic elsewhere
    /// cannot have left the file half-written.
    pub(crate) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // --- settings ---------------------------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO settings(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        );
    }

    pub fn delete_setting(&self, key: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM settings WHERE key = ?1", [key]);
    }

    /// Persist the canonical selected identity and its two legacy projections atomically. Older
    /// releases still read `data_sync_id` / `account_json`; keeping all three in one transaction
    /// prevents a restart from pairing one channel's request delegation with another's display.
    pub fn set_auth_identity(
        &self,
        session_cookie: &str,
        selected_json: &str,
        data_sync_id: Option<&str>,
        account_json: &str,
    ) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('session_cookie', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [session_cookie],
        )?;
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('selected_identity_json', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [selected_json],
        )?;
        if let Some(id) = data_sync_id {
            tx.execute(
                "INSERT INTO settings(key, value) VALUES('data_sync_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [id],
            )?;
        } else {
            tx.execute("DELETE FROM settings WHERE key = 'data_sync_id'", [])?;
        }
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('account_json', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [account_json],
        )?;
        tx.execute("DELETE FROM settings WHERE key = 'account_selection_pending'", [])?;
        tx.commit()
    }

    /// Persist an authenticated cookie while deliberately leaving the account unfinished. Keeping
    /// the marker and removal of stale identity projections in the same transaction means a crash
    /// during the required picker cannot restart into YouTube's default channel silently.
    pub fn set_pending_auth_selection(
        &self,
        session_cookie: &str,
        account_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('session_cookie', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [session_cookie],
        )?;
        for key in ["selected_identity_json", "data_sync_id", "account_json"] {
            tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        }
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('account_selection_pending', 'true')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        if let Some(id) = account_id {
            tx.execute(
                "INSERT INTO settings(key, value) VALUES('active_account', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [id],
            )?;
        }
        tx.commit()
    }

    pub fn clear_auth_identity(&self) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        for key in
            ["selected_identity_json", "data_sync_id", "account_json", "account_selection_pending"]
        {
            tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        }
        tx.commit()
    }

    // --- saved Google accounts (multi-account) ------------------------------------------------

    /// Insert or refresh a saved account. `added_at` is deliberately not updated on conflict: a
    /// re-login refreshes the cookie and identity, not the account's place in the list order.
    pub fn upsert_account(&self, account: &StoredAccount) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO accounts(id, session_cookie, data_sync_id, selected_identity_json, \
             account_json, visitor_data, added_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET \
                session_cookie = excluded.session_cookie, \
                data_sync_id = excluded.data_sync_id, \
                selected_identity_json = excluded.selected_identity_json, \
                account_json = excluded.account_json, \
                visitor_data = excluded.visitor_data",
            rusqlite::params![
                account.id,
                account.session_cookie,
                account.data_sync_id,
                account.selected_identity_json,
                account.account_json,
                account.visitor_data,
                account.added_at
            ],
        )?;
        Ok(())
    }

    /// Saved accounts, oldest first. No raw auth material leaves this function's callers except
    /// back into the transport; the UI only ever sees display fields plus the opaque id.
    pub fn list_accounts(&self) -> Vec<StoredAccount> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, session_cookie, data_sync_id, selected_identity_json, account_json, \
             visitor_data, added_at FROM accounts ORDER BY added_at, id",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(StoredAccount {
                    id: r.get(0)?,
                    session_cookie: r.get(1)?,
                    data_sync_id: r.get(2)?,
                    selected_identity_json: r.get(3)?,
                    account_json: r.get(4)?,
                    visitor_data: r.get(5)?,
                    added_at: r.get(6)?,
                })
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// One saved account by id, or `None` when it isn't saved.
    pub fn get_account(&self, id: &str) -> Option<StoredAccount> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, session_cookie, data_sync_id, selected_identity_json, account_json, \
             visitor_data, added_at FROM accounts WHERE id = ?1",
            [id],
            |r| {
                Ok(StoredAccount {
                    id: r.get(0)?,
                    session_cookie: r.get(1)?,
                    data_sync_id: r.get(2)?,
                    selected_identity_json: r.get(3)?,
                    account_json: r.get(4)?,
                    visitor_data: r.get(5)?,
                    added_at: r.get(6)?,
                })
            },
        )
        .ok()
    }

    /// Write a rotated cookie jar into one account's row, leaving its identity fields alone.
    /// Nothing happens when the row is gone (the account was removed while a request was in
    /// flight), which is what should happen.
    pub fn update_account_cookie(&self, id: &str, session_cookie: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn
            .execute("UPDATE accounts SET session_cookie = ?1 WHERE id = ?2", [session_cookie, id]);
    }

    /// Delete a saved account row. Whether removing it signs the app out is the caller's decision
    /// (see `AppState::remove_google_account`), not this row's.
    pub fn remove_account(&self, id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM accounts WHERE id = ?1", [id]);
    }

    /// Write a saved account's fields into the active-account `settings` projections and flip the
    /// `active_account` pointer, atomically (used when switching to a saved account), so a crash
    /// mid-switch can't restart into projections for one account and a pointer for another.
    /// Clears `account_selection_pending`: a completed account needs no channel pick.
    pub fn restore_account(&self, account: &StoredAccount) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('session_cookie', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&account.session_cookie],
        )?;
        for (key, value) in [
            ("data_sync_id", account.data_sync_id.as_deref()),
            ("selected_identity_json", account.selected_identity_json.as_deref()),
            ("account_json", account.account_json.as_deref()),
            ("visitor_data", account.visitor_data.as_deref()),
        ] {
            match value {
                Some(value) => {
                    tx.execute(
                        "INSERT INTO settings(key, value) VALUES(?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        [key, value],
                    )?;
                }
                None => {
                    tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO settings(key, value) VALUES('active_account', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [&account.id],
        )?;
        tx.execute("DELETE FROM settings WHERE key = 'account_selection_pending'", [])?;
        tx.commit()
    }

    pub fn all_settings(&self) -> Vec<(String, String)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare("SELECT key, value FROM settings") {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    // --- stream url cache -------------------------------------------------------------------

    /// Return the cached URL only if still valid (`expires_at` in the future). context/11.
    pub fn get_stream(&self, video_id: &str, now: i64) -> Option<CachedStream> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT url, itag, expires_at, loudness_db, is_video, ping_url, ping_client, client FROM stream_url_cache WHERE video_id = ?1 AND expires_at > ?2",
            rusqlite::params![video_id, now],
            |r| {
                Ok(CachedStream {
                    url: r.get(0)?,
                    itag: r.get(1)?,
                    expires_at: r.get(2)?,
                    loudness_db: r.get(3)?,
                    is_video: r.get(4)?,
                    ping_url: r.get(5)?,
                    ping_client: r.get(6)?,
                    client: r.get(7)?,
                })
            },
        )
        .ok()
    }

    /// Drop a cached URL (e.g. it 403'd on the real GET). context/06 §2.
    pub fn evict_stream(&self, video_id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM stream_url_cache WHERE video_id = ?1", [video_id]);
    }

    /// Cache one resolved URL, and drop every entry that has already expired.
    ///
    /// The prune rides along with the insert (same shape as [`Db::record_play`]) because nothing
    /// else ever deleted a dead row: `get_stream` filters them out but leaves them, so the table
    /// only ever grew. Measured on a real install before this: 1803 rows / 2.5 MB, nearly all of
    /// them URLs that expired hours or weeks ago.
    pub fn put_stream(&self, video_id: &str, row: &CachedStream, now: i64) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO stream_url_cache(video_id, url, itag, expires_at, loudness_db, is_video, ping_url, ping_client, client) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(video_id) DO UPDATE SET url = excluded.url, itag = excluded.itag, expires_at = excluded.expires_at, loudness_db = excluded.loudness_db, is_video = excluded.is_video, ping_url = excluded.ping_url, ping_client = excluded.ping_client, client = excluded.client",
            rusqlite::params![
                video_id,
                row.url,
                row.itag,
                row.expires_at,
                row.loudness_db,
                row.is_video,
                row.ping_url,
                row.ping_client,
                row.client
            ],
        );
        let _ = conn.execute("DELETE FROM stream_url_cache WHERE expires_at <= ?1", [now]);
    }

    /// Wipe the whole URL cache (settings "Clear caches"). context/11.
    pub fn clear_stream_cache(&self) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM stream_url_cache", []);
        let _ = conn.execute(CLEAR_LYRICS, []);
        // Spared like a pinned lyrics source: a pick made by hand is the user's, not a cache.
        let _ = conn.execute("DELETE FROM import_matches WHERE manual = 0", []);
    }

    pub fn get_import_match(&self, key: &str) -> Option<ImportMatch> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT video_id, song_json, tier, candidates_json, manual, updated_at
             FROM import_matches WHERE key = ?1",
            [key],
            |r| {
                Ok(ImportMatch {
                    video_id: r.get(0)?,
                    song_json: r.get(1)?,
                    tier: r.get(2)?,
                    candidates_json: r.get(3)?,
                    manual: r.get::<_, i64>(4)? != 0,
                    updated_at: r.get(5)?,
                })
            },
        )
        .ok()
    }

    pub fn put_import_match(&self, key: &str, m: &ImportMatch) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO import_matches(key, video_id, song_json, tier, candidates_json, manual,
                updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET video_id = excluded.video_id,
                song_json = excluded.song_json, tier = excluded.tier,
                candidates_json = excluded.candidates_json, manual = excluded.manual,
                updated_at = excluded.updated_at",
            rusqlite::params![
                key,
                m.video_id,
                m.song_json,
                m.tier,
                m.candidates_json,
                m.manual as i64,
                m.updated_at
            ],
        );
    }

    /// The cached title lookup for a video, or `None` when there is none or it is a negative
    /// verdict older than [`RECOVER_NEGATIVE_TTL_SECS`] (worth asking again). Read by the recovery
    /// assistant (`playlist_tools::recover`).
    pub fn recover_title_get(&self, video_id: &str) -> Option<CachedTitle> {
        self.recover_title_get_at(video_id, now_secs())
    }

    /// [`Db::recover_title_get`] against a given clock, so the expiry is testable.
    fn recover_title_get_at(&self, video_id: &str, now: i64) -> Option<CachedTitle> {
        let conn = self.0.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT title, artists, source, found, checked_at
                 FROM recover_titles WHERE video_id = ?1",
                [video_id],
                |r| {
                    Ok(CachedTitle {
                        title: r.get(0)?,
                        artists: r.get(1)?,
                        source: r.get(2)?,
                        found: r.get::<_, i64>(3)? != 0,
                        checked_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()?;
        (row.found || now - row.checked_at <= RECOVER_NEGATIVE_TTL_SECS).then_some(row)
    }

    /// Store (or replace) a video's title lookup.
    pub fn recover_title_put(&self, video_id: &str, t: &CachedTitle) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO recover_titles(video_id, title, artists, source, found, checked_at)
                VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(video_id) DO UPDATE SET title = excluded.title,
                artists = excluded.artists, source = excluded.source, found = excluded.found,
                checked_at = excluded.checked_at",
            rusqlite::params![video_id, t.title, t.artists, t.source, t.found as i64, t.checked_at],
        );
    }

    /// Drop cached lyrics only, leaving stream URLs alone. Changing which providers are allowed
    /// has to invalidate what earlier ones already answered, or the setting appears to do nothing
    /// on every track whose lyrics were already fetched (cache hits never expire).
    pub fn clear_lyrics_cache(&self) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(CLEAR_LYRICS, []);
    }

    /// Forget one song's lyrics, a hand-picked source included.
    pub fn delete_lyrics(&self, video_id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM lyrics_cache WHERE video_id = ?1", [video_id]);
    }

    // --- lyrics cache -----------------------------------------------------------------------

    /// Cached lyrics JSON for a track. `Some(None)` = a cached "no lyrics" verdict (NULL row),
    /// still valid; misses expire after `miss_ttl` secs while hits live forever.
    pub fn get_lyrics(&self, video_id: &str, now: i64, miss_ttl: i64) -> Option<Option<String>> {
        let conn = self.0.lock().unwrap();
        let (lyrics, fetched_at): (Option<String>, i64) = conn
            .query_row(
                "SELECT lyrics, fetched_at FROM lyrics_cache WHERE video_id = ?1",
                [video_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        if lyrics.is_none() && now - fetched_at > miss_ttl {
            return None; // stale negative result → refetch
        }
        Some(lyrics)
    }

    /// `lyrics = None` records a "no lyrics found" verdict.
    pub fn put_lyrics(&self, video_id: &str, lyrics: Option<&str>, now: i64) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO lyrics_cache(video_id, lyrics, fetched_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(video_id) DO UPDATE SET lyrics = excluded.lyrics, fetched_at = excluded.fetched_at",
            rusqlite::params![video_id, lyrics, now],
        );
    }

    // --- play history (the On Repeat playlist) ------------------------------------------------

    /// Record one completed play and drop everything that has fallen out of the window, so the
    /// table stays bounded at roughly a month of listening whether or not anyone opens the
    /// playlist. `song_json` is the serialized `SongItem`, kept per row so the playlist can be
    /// rebuilt without asking YouTube for metadata it already gave us.
    pub fn record_play(&self, video_id: &str, song_json: &str, now: i64, window: i64) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO plays(video_id, played_at, song_json) VALUES(?1, ?2, ?3)",
            rusqlite::params![video_id, now, song_json],
        );
        let _ = conn.execute("DELETE FROM plays WHERE played_at < ?1", [now - window]);
    }

    /// The most-played songs since `since`, as `(song_json, play_count)` ranked by plays and then
    /// by recency. Each row's JSON comes from that song's latest play: SQLite resolves a bare
    /// column against the row matching the single `max()` in the query.
    pub fn top_plays(&self, since: i64, limit: usize) -> Vec<(String, i64)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT song_json, COUNT(*) AS plays, MAX(played_at) AS last FROM plays
             WHERE played_at >= ?1
             GROUP BY video_id
             ORDER BY plays DESC, last DESC
             LIMIT ?2",
        ) {
            if let Ok(rows) = stmt
                .query_map(rusqlite::params![since, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Play count per videoId since `since`. [`Db::top_plays`] answers "what are my 20 most played
    /// songs"; this answers "how many times have I played each of these", which is what sorting an
    /// arbitrary playlist by plays needs. Same table, so the same trailing window applies.
    pub fn play_counts(&self, since: i64) -> Vec<(String, i64)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn
            .prepare("SELECT video_id, COUNT(*) FROM plays WHERE played_at >= ?1 GROUP BY video_id")
        {
            if let Ok(rows) = stmt.query_map([since], |r| Ok((r.get(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    // --- playlist membership index (which of your playlists hold a track) ----------------------
    // Populated by `commands::sync_playlist_index`, which walks the library's owned playlists.
    // Nothing here talks to YouTube; it is the answer, cached, so a track list can draw the
    // "saved" mark on its first row instead of after a round-trip per song.

    /// Replace one playlist's tracks. Delete-then-insert, not an upsert: a removal made on another
    /// device only disappears if the rows the crawl no longer saw go away with it. `first_seen`
    /// survives the rewrite (see [`replace_playlist_rows`]).
    pub fn set_playlist_tracks(&self, playlist_id: &str, video_ids: &[String]) {
        let rows: Vec<(&str, Option<&str>)> =
            video_ids.iter().map(|v| (v.as_str(), None)).collect();
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        if replace_playlist_rows(&tx, playlist_id, &rows, now_secs()).is_ok() {
            let _ = tx.commit();
        }
    }

    /// One track added to one playlist, so an add made here shows its mark without a re-crawl.
    /// Added here, so it was first seen now; a row the index already had keeps its date.
    pub fn add_playlist_track(&self, playlist_id: &str, video_id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO playlist_track(playlist_id, video_id, first_seen) \
             VALUES(?1, ?2, ?3)",
            rusqlite::params![playlist_id, video_id, now_secs()],
        );
    }

    pub fn remove_playlist_track(&self, playlist_id: &str, video_id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM playlist_track WHERE playlist_id = ?1 AND video_id = ?2",
            [playlist_id, video_id],
        );
    }

    /// Forget one playlist's index rows, alerts and sync record. Its snapshots stay: they are the
    /// playlist's history (and its backups' source), which outlives the playlist itself.
    pub fn forget_playlist(&self, playlist_id: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM playlist_track WHERE playlist_id = ?1", [playlist_id]);
        let _ = conn.execute("DELETE FROM playlist_alert WHERE playlist_id = ?1", [playlist_id]);
        let _ = conn.execute("DELETE FROM playlist_sync WHERE playlist_id = ?1", [playlist_id]);
    }

    /// Drop every playlist the crawl no longer saw: deleted, unsaved, or no longer owned. An
    /// empty list means nothing was indexed, which is the same thing as an empty index.
    /// Snapshots are kept, as in [`Db::forget_playlist`].
    pub fn retain_playlists(&self, keep: &[String]) {
        let conn = self.0.lock().unwrap();
        if keep.is_empty() {
            let _ = conn.execute("DELETE FROM playlist_track", []);
            let _ = conn.execute(ACCOUNT_ALERTS_DELETE, []);
            let _ = conn.execute(ACCOUNT_SYNC_DELETE, []);
            return;
        }
        let holes = vec!["?"; keep.len()].join(",");
        let _ = conn.execute(
            &format!("DELETE FROM playlist_track WHERE playlist_id NOT IN ({holes})"),
            rusqlite::params_from_iter(keep.iter()),
        );
        let _ = conn.execute(
            &format!("{ACCOUNT_ALERTS_DELETE} AND playlist_id NOT IN ({holes})"),
            rusqlite::params_from_iter(keep.iter()),
        );
        let _ = conn.execute(
            &format!("{ACCOUNT_SYNC_DELETE} AND playlist_id NOT IN ({holes})"),
            rusqlite::params_from_iter(keep.iter()),
        );
    }

    /// videoId → the playlists holding it, the ones on this machine included (as their
    /// `LOCALPLAYLIST:` browseIds), so the saved mark and "Remove from this playlist" treat both
    /// kinds alike. ponytail: the whole table in one go, like `local_tracks`, since an
    /// owned-playlist library is thousands of rows and the UI needs random access to it on every
    /// row it draws.
    pub fn playlist_memberships(&self) -> std::collections::HashMap<String, Vec<String>> {
        let conn = self.0.lock().unwrap();
        let mut out: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let sql = format!(
            "SELECT video_id, playlist_id FROM playlist_track UNION ALL \
             SELECT video_id, '{}' || playlist_id FROM local_playlist_tracks",
            crate::state::LOCAL_PLAYLIST_PREFIX
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for (video_id, playlist_id) in rows.flatten() {
                    out.entry(video_id).or_default().push(playlist_id);
                }
            }
        }
        out
    }

    /// The index is per-account, so signing out or switching channel empties it. The playlists on
    /// this machine belong to no account and are not in that table. Snapshots are kept: they are
    /// history, filed under the account they were taken with.
    pub fn clear_playlist_index(&self) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM playlist_track", []);
        let _ = conn.execute(ACCOUNT_ALERTS_DELETE, []);
        let _ = conn.execute(ACCOUNT_SYNC_DELETE, []);
    }

    // --- songs in the index, and the monitor's alerts (playlist_tools::monitor) ---------------

    /// What the index holds for one playlist: `(videoId, song_json)`, json NULL until a sync wrote
    /// it. The monitor compares a fresh crawl against this before it replaces it.
    pub fn playlist_songs(&self, playlist_id: &str) -> Vec<(String, Option<String>)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT video_id, song_json FROM playlist_track WHERE playlist_id = ?1")
        {
            if let Ok(rows) = stmt.query_map([playlist_id], |r| Ok((r.get(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Replace one playlist's index rows with these songs, metadata included. `first_seen`
    /// survives (see [`replace_playlist_rows`]).
    #[cfg(test)]
    pub fn set_playlist_songs(&self, playlist_id: &str, songs: &[(String, String)]) {
        self.set_playlist_songs_at(playlist_id, songs, now_secs());
    }

    /// [`Db::set_playlist_songs`] with the clock passed in. A track new to the playlist is first
    /// seen `now` only when the playlist was synced before; on its first read nobody knows when
    /// its tracks arrived, so they stay NULL. Write the index before `set_playlist_sync`.
    pub fn set_playlist_songs_at(&self, playlist_id: &str, songs: &[(String, String)], now: i64) {
        let rows: Vec<(&str, Option<&str>)> =
            songs.iter().map(|(v, j)| (v.as_str(), Some(j.as_str()))).collect();
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        if replace_playlist_rows(&tx, playlist_id, &rows, now).is_ok() {
            let _ = tx.commit();
        }
    }

    /// A read cut short (a failed page, the page cap): write down the songs it did see, metadata
    /// included, and leave every other row of the playlist alone. Deleting the unread tail would
    /// have the next complete read find it "new" and stamp it `first_seen = now`. A row already
    /// there keeps its `first_seen`; one new to it is first seen `now` when the playlist was synced
    /// before, NULL otherwise, as in [`Db::set_playlist_songs_at`].
    pub fn upsert_playlist_songs_at(
        &self,
        playlist_id: &str,
        songs: &[(String, String)],
        now: i64,
    ) {
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        let written = (|| -> rusqlite::Result<()> {
            let synced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlist_sync WHERE playlist_id = ?1)",
                [playlist_id],
                |r| r.get(0),
            )?;
            let fresh = synced.then_some(now);
            for (video_id, json) in songs {
                tx.execute(
                    "INSERT INTO playlist_track(playlist_id, video_id, song_json, first_seen) \
                     VALUES(?1, ?2, ?3, ?4) \
                     ON CONFLICT(playlist_id, video_id) \
                     DO UPDATE SET song_json = excluded.song_json",
                    rusqlite::params![playlist_id, video_id, json, fresh],
                )?;
            }
            Ok(())
        })();
        if written.is_ok() {
            let _ = tx.commit();
        }
    }

    /// The dates the Data API gave for when each track was added to the playlist (`videoId` →
    /// epoch seconds), written on the playlist's index rows. A track the map leaves out keeps the
    /// date it had (an InnerTube rewrite keeps them too, see [`replace_playlist_rows`]); a row the
    /// index does not hold is not created. Write the index first.
    pub fn set_playlist_added_at(
        &self,
        playlist_id: &str,
        dates: &std::collections::HashMap<String, i64>,
    ) {
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        let written = (|| -> rusqlite::Result<()> {
            let mut stmt = tx.prepare(
                "UPDATE playlist_track SET added_at = ?3 WHERE playlist_id = ?1 AND video_id = ?2",
            )?;
            for (video_id, at) in dates {
                stmt.execute(rusqlite::params![playlist_id, video_id, at])?;
            }
            Ok(())
        })();
        if written.is_ok() {
            let _ = tx.commit();
        }
    }

    /// `videoId` → the earliest known date it was added to one of your playlists (epoch seconds,
    /// from the Data API's `snippet.publishedAt`). Liked Music is left out, as in the "In your
    /// playlists" view; tracks with no known date are absent.
    pub fn playlist_added_dates(&self) -> std::collections::HashMap<String, i64> {
        let conn = self.0.lock().unwrap();
        let mut out = std::collections::HashMap::new();
        let liked = crate::commands::LIKED_MUSIC_ID;
        if let Ok(mut stmt) = conn.prepare(
            "SELECT video_id, MIN(added_at) FROM playlist_track \
             WHERE added_at IS NOT NULL AND playlist_id <> ?1 GROUP BY video_id",
        ) {
            if let Ok(rows) = stmt.query_map([liked], |r| Ok((r.get::<_, String>(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// One track now in a playlist, with its metadata (an add made in this app), first seen now
    /// unless the index already had it.
    pub fn put_playlist_song(&self, playlist_id: &str, video_id: &str, json: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO playlist_track(playlist_id, video_id, song_json, first_seen) \
             VALUES(?1, ?2, ?3, ?4) \
             ON CONFLICT(playlist_id, video_id) DO UPDATE SET song_json = excluded.song_json",
            rusqlite::params![playlist_id, video_id, json, now_secs()],
        );
    }

    /// Every track in every playlist of yours that has its metadata, the ones on this machine
    /// included: `(videoId, playlist id, song_json, first_seen)`. `first_seen` is in epoch seconds,
    /// null for a track held since before tracking began; a local playlist's is when it was added.
    pub fn indexed_songs(&self) -> Vec<(String, String, String, Option<i64>)> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "SELECT video_id, playlist_id, song_json, first_seen FROM playlist_track \
             WHERE song_json IS NOT NULL \
             UNION ALL SELECT video_id, '{}' || playlist_id, song_json, added_at \
             FROM local_playlist_tracks",
            crate::state::LOCAL_PLAYLIST_PREFIX
        );
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Record an alert unless the same one is already there (dismissed ones included): one per
    /// playlist, track and kind, keyed by [`alert_dedupe_key`] with no scope. The pre-v3 `gone`
    /// is filed as `removed`.
    #[cfg(test)]
    pub fn add_playlist_alert(
        &self,
        playlist_id: &str,
        video_id: &str,
        kind: &str,
        song_json: Option<&str>,
        at: i64,
    ) {
        let kind = alert_kind(kind);
        let key = alert_dedupe_key(playlist_id, video_id, kind, None);
        self.insert_alert(&NewAlert {
            playlist_id,
            video_id,
            kind,
            song_json,
            at,
            from_pos: None,
            to_pos: None,
            dedupe_key: &key,
        });
    }

    /// The alerts not dismissed, newest first: `(playlist id, videoId, kind, song_json, at)`.
    #[cfg(test)]
    pub fn playlist_alerts(&self) -> Vec<(String, String, String, Option<String>, i64)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT playlist_id, video_id, kind, song_json, at FROM playlist_alert \
             WHERE dismissed = 0 ORDER BY at DESC, playlist_id, video_id, id DESC",
        ) {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Dismiss every alert of this kind about this track in this playlist (a repeated event can
    /// have several rows). Dismissed is seen too.
    pub fn dismiss_playlist_alert(&self, playlist_id: &str, video_id: &str, kind: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "UPDATE playlist_alert SET dismissed = 1, seen = 1 \
             WHERE playlist_id = ?1 AND video_id = ?2 AND kind = ?3",
            [playlist_id, video_id, alert_kind(kind)],
        );
    }

    // --- playlists on this machine (issue #251) -----------------------------------------------
    // The user's own data, so unlike the cache writes above every write here answers whether it
    // happened: a playlist edit that silently did nothing is a lost edit.

    pub fn create_local_playlist(&self, title: &str, now: i64) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO local_playlists(title, created_at, updated_at) VALUES(?1, ?2, ?2)",
            rusqlite::params![title, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Every playlist, the most recently changed first (YouTube's own library order).
    pub fn local_playlists(&self) -> Vec<LocalPlaylist> {
        self.query_local_playlists(None)
    }

    pub fn local_playlist(&self, id: i64) -> Option<LocalPlaylist> {
        self.query_local_playlists(Some(id)).pop()
    }

    fn query_local_playlists(&self, id: Option<i64>) -> Vec<LocalPlaylist> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "SELECT p.id, p.title, p.description,
                    (SELECT COUNT(*) FROM local_playlist_tracks t WHERE t.playlist_id = p.id),
                    (SELECT song_json FROM local_playlist_tracks t WHERE t.playlist_id = p.id
                     ORDER BY t.position, t.id LIMIT 1)
             FROM local_playlists p {}
             ORDER BY p.updated_at DESC, p.id DESC",
            if id.is_some() { "WHERE p.id = ?1" } else { "" }
        );
        let row = |r: &rusqlite::Row| {
            Ok(LocalPlaylist {
                id: r.get(0)?,
                title: r.get(1)?,
                description: r.get(2)?,
                count: r.get(3)?,
                first_song: r.get(4)?,
            })
        };
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let rows = match id {
                Some(id) => stmt.query_map([id], row),
                None => stmt.query_map([], row),
            };
            if let Ok(rows) = rows {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// One playlist's tracks in playlist order (the order added, unless reordered since), as
    /// `(row id, song_json)`.
    pub fn local_playlist_tracks(&self, id: i64) -> Vec<(i64, String)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, song_json FROM local_playlist_tracks WHERE playlist_id = ?1 \
             ORDER BY position, id",
        ) {
            if let Ok(rows) = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Append `(video_id, song_json)` rows, answering per row whether it went in: `false` is a
    /// track the playlist already holds, which is refused the way YouTube refuses one. One
    /// transaction, so a bulk add of a whole album is one fsync and lands all or nothing.
    pub fn add_local_playlist_tracks(
        &self,
        id: i64,
        songs: &[(String, String)],
        now: i64,
    ) -> rusqlite::Result<Vec<bool>> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let exists: bool =
            tx.query_row("SELECT COUNT(*) FROM local_playlists WHERE id = ?1", [id], |r| {
                r.get::<_, i64>(0).map(|n| n > 0)
            })?;
        if !exists {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let mut added = Vec::with_capacity(songs.len());
        for (video_id, json) in songs {
            let n = tx.execute(
                "INSERT OR IGNORE INTO local_playlist_tracks(playlist_id, video_id, song_json, \
                 added_at, position) VALUES(?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(position), 0) \
                 + 1 FROM local_playlist_tracks WHERE playlist_id = ?1))",
                rusqlite::params![id, video_id, json, now],
            )?;
            added.push(n > 0);
        }
        if added.contains(&true) {
            tx.execute("UPDATE local_playlists SET updated_at = ?1 WHERE id = ?2", [now, id])?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// Drop rows by their row id. Scoped to the playlist, so a stale id from another list can
    /// never take out someone else's row.
    pub fn remove_local_playlist_tracks(
        &self,
        id: i64,
        rows: &[i64],
        now: i64,
    ) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        for row in rows {
            tx.execute(
                "DELETE FROM local_playlist_tracks WHERE playlist_id = ?1 AND id = ?2",
                [id, *row],
            )?;
        }
        tx.execute("UPDATE local_playlists SET updated_at = ?1 WHERE id = ?2", [now, id])?;
        tx.commit()
    }

    /// Put a playlist's rows in the order `rows` names them. Rows it leaves out keep their order
    /// and follow the named ones, and an id that isn't in this playlist touches nothing, so a
    /// stale order from a list that changed meanwhile can't lose or steal a row.
    pub fn reorder_local_playlist(&self, id: i64, rows: &[i64], now: i64) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let current: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM local_playlist_tracks WHERE playlist_id = ?1 ORDER BY position, id",
            )?;
            let ids = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ids
        };
        let named: Vec<i64> = rows.iter().copied().filter(|r| current.contains(r)).collect();
        let rest = current.iter().copied().filter(|r| !named.contains(r));
        for (pos, row) in named.iter().copied().chain(rest).enumerate() {
            tx.execute(
                "UPDATE local_playlist_tracks SET position = ?1 WHERE playlist_id = ?2 AND id = ?3",
                [pos as i64 + 1, id, row],
            )?;
        }
        tx.execute("UPDATE local_playlists SET updated_at = ?1 WHERE id = ?2", [now, id])?;
        tx.commit()
    }

    // --- playlist tools journal (playlist_tools::journal) -------------------------------------

    /// Record an operation and prune the journal to the newest `PLAYLIST_OPS_KEPT`. Answers its id.
    pub fn record_playlist_op(
        &self,
        kind: &str,
        account: Option<&str>,
        summary_json: &str,
        inverse_json: &str,
        now: i64,
    ) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO playlist_ops(kind, account, summary_json, inverse_json, created_at) \
             VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![kind, account, summary_json, inverse_json, now],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "DELETE FROM playlist_ops WHERE id NOT IN \
             (SELECT id FROM playlist_ops ORDER BY id DESC LIMIT ?1)",
            [PLAYLIST_OPS_KEPT],
        )?;
        Ok(id)
    }

    /// The journal, newest first.
    pub fn playlist_ops(&self) -> Vec<PlaylistOpRow> {
        self.query_playlist_ops(None)
    }

    pub fn playlist_op(&self, id: i64) -> Option<PlaylistOpRow> {
        self.query_playlist_ops(Some(id)).pop()
    }

    fn query_playlist_ops(&self, id: Option<i64>) -> Vec<PlaylistOpRow> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "SELECT id, kind, account, summary_json, inverse_json, created_at, undone_at \
             FROM playlist_ops {} ORDER BY id DESC",
            if id.is_some() { "WHERE id = ?1" } else { "" }
        );
        let row = |r: &rusqlite::Row| {
            Ok(PlaylistOpRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                account: r.get(2)?,
                summary_json: r.get(3)?,
                inverse_json: r.get(4)?,
                created_at: r.get(5)?,
                undone_at: r.get(6)?,
            })
        };
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let rows = match id {
                Some(id) => stmt.query_map([id], row),
                None => stmt.query_map([], row),
            };
            if let Ok(rows) = rows {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Mark an operation undone. Answers `false` when it already was (or is gone), so two undo
    /// clicks racing each other can't both replay the inverse.
    pub fn mark_playlist_op_undone(&self, id: i64, now: i64) -> rusqlite::Result<bool> {
        let conn = self.0.lock().unwrap();
        let n = conn.execute(
            "UPDATE playlist_ops SET undone_at = ?1 WHERE id = ?2 AND undone_at IS NULL",
            [now, id],
        )?;
        Ok(n > 0)
    }

    /// Rename and/or re-describe. `None` leaves that field as it is. Errors when there is no such
    /// playlist, rather than reporting an edit nothing received.
    pub fn edit_local_playlist(
        &self,
        id: i64,
        title: Option<&str>,
        description: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        let n = conn.execute(
            "UPDATE local_playlists SET title = COALESCE(?1, title),
                 description = COALESCE(?2, description), updated_at = ?3 WHERE id = ?4",
            rusqlite::params![title, description, now, id],
        )?;
        if n == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    pub fn delete_local_playlist(&self, id: i64) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM local_playlist_tracks WHERE playlist_id = ?1", [id])?;
        tx.execute("DELETE FROM local_playlists WHERE id = ?1", [id])?;
        tx.commit()
    }

    // --- local music library (local.rs) -------------------------------------------------------

    /// Every known file with its recorded mtime — the scanner re-reads tags only where it differs.
    pub fn local_mtimes(&self) -> std::collections::HashMap<String, i64> {
        let conn = self.0.lock().unwrap();
        let mut out = std::collections::HashMap::new();
        if let Ok(mut stmt) = conn.prepare("SELECT path, mtime FROM local_tracks") {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Upsert a batch in one transaction. SQLite fsyncs per statement otherwise, which is the
    /// difference between a first scan taking a second and taking minutes.
    pub fn put_local_tracks(&self, tracks: &[LocalTrack]) {
        if tracks.is_empty() {
            return;
        }
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        for t in tracks {
            let _ = tx.execute(
                LOCAL_TRACK_UPSERT,
                rusqlite::params![
                    t.path,
                    t.title,
                    t.artist,
                    t.album,
                    t.album_key,
                    t.album_artist,
                    t.track_no,
                    t.duration_secs,
                    t.cover,
                    t.mtime,
                    t.disc_no
                ],
            );
        }
        let _ = tx.commit();
    }

    /// Forget files that are no longer on disk (the user deleted or moved them).
    pub fn delete_local_tracks(&self, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return };
        for p in paths {
            let _ = tx.execute("DELETE FROM local_tracks WHERE path = ?1", [p]);
        }
        let _ = tx.commit();
    }

    /// All tracks, or one album's, in album order. ponytail: loads the whole table — a personal
    /// collection is thousands of rows, so paging it would buy nothing.
    pub fn local_tracks(&self, album_key: Option<&str>) -> Vec<LocalTrack> {
        let conn = self.0.lock().unwrap();
        let sql =
            "SELECT path, title, artist, album, album_key, album_artist, track_no, duration_secs, cover, mtime, disc_no
                   FROM local_tracks {WHERE}";
        let sql =
            sql.replace("{WHERE}", if album_key.is_some() { "WHERE album_key = ?1" } else { "" });
        let mut out = Vec::new();
        let row = |r: &rusqlite::Row| {
            Ok(LocalTrack {
                path: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                album: r.get(3)?,
                album_key: r.get(4)?,
                album_artist: r.get(5)?,
                track_no: r.get(6)?,
                duration_secs: r.get(7)?,
                cover: r.get(8)?,
                mtime: r.get(9)?,
                disc_no: r.get(10)?,
            })
        };
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let rows = match album_key {
                Some(k) => stmt.query_map([k], row),
                None => stmt.query_map([], row),
            };
            if let Ok(rows) = rows {
                out.extend(rows.flatten());
            }
        }
        // Disc before track, since every disc numbers from 1 (issue #315). The folder sits between
        // them for a rip with no disc tag: CD1/ and CD2/ keep their tracks apart instead of
        // interleaving them. It changes nothing for an album that lives in one folder.
        //
        // The album itself goes right after the title, or two albums sharing one ("Greatest Hits")
        // are dealt out a disc at a time. With no album artist the key is the folder's digest
        // (`local::tagged_album_key`), so the folder stands in for it there: it sorts by name,
        // which keeps an untagged CD1/ ahead of CD2/.
        fn order(
            t: &LocalTrack,
        ) -> (&str, &str, Option<&std::path::Path>, i64, Option<&std::path::Path>, i64, &str)
        {
            let dir = std::path::Path::new(&t.path).parent();
            let (key, key_dir) = match t.album_artist {
                Some(_) => (t.album_key.as_str(), None),
                None => ("", dir),
            };
            (&t.album, key, key_dir, t.disc_no, dir, t.track_no, &t.title)
        }
        out.sort_by(|a, b| order(a).cmp(&order(b)));
        out
    }
}

const LOCAL_TRACK_UPSERT: &str =
    "INSERT INTO local_tracks(path, title, artist, album, album_key, album_artist, track_no, duration_secs, cover, mtime, disc_no)
     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
     ON CONFLICT(path) DO UPDATE SET title = excluded.title, artist = excluded.artist,
        album = excluded.album, album_key = excluded.album_key,
        album_artist = excluded.album_artist, track_no = excluded.track_no,
        duration_secs = excluded.duration_secs, cover = excluded.cover, mtime = excluded.mtime,
        disc_no = excluded.disc_no";

/// One file in the local library. Tag data as read at scan time; `mtime` is the change detector.
#[derive(Debug, Clone)]
pub struct LocalTrack {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Stable, human-readable album id fragment (`artist--album`, sanitized). See `local.rs`.
    pub album_key: String,
    /// The AlbumArtist tag, when the file has one. What an album is credited to, even when its
    /// tracks name different performers.
    pub album_artist: Option<String>,
    pub track_no: i64,
    /// 0 when the file has no disc tag.
    pub disc_no: i64,
    pub duration_secs: i64,
    /// Absolute path to the cover image (extracted or found next to the files).
    pub cover: Option<String>,
    pub mtime: i64,
}

/// A playlist kept on this machine, as the library grid and the playlist header need it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalPlaylist {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub count: i64,
    /// The first track's stored `SongItem`, whose artwork stands in for a cover nobody picked.
    pub first_song: Option<String>,
}

/// Alerts about account playlists: what an account change or a full re-index clears. The ones
/// about playlists on this machine belong to no account and stay.
const ACCOUNT_ALERTS_DELETE: &str =
    "DELETE FROM playlist_alert WHERE playlist_id NOT LIKE 'LOCALPLAYLIST:%'";

/// The account playlists' sync records, cleared along with their alerts. Snapshots are not: they
/// are history, and outlive both the playlist and the sign-in.
const ACCOUNT_SYNC_DELETE: &str =
    "DELETE FROM playlist_sync WHERE playlist_id NOT LIKE 'LOCALPLAYLIST:%'";

/// How many playlist-tool operations the journal keeps (the undo history's length).
pub const PLAYLIST_OPS_KEPT: i64 = 20;

/// One journal row. The two JSON columns belong to `playlist_tools::journal`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistOpRow {
    pub id: i64,
    pub kind: String,
    /// The account it ran under: an undo only replays there.
    pub account: Option<String>,
    pub summary_json: String,
    pub inverse_json: String,
    pub created_at: i64,
    pub undone_at: Option<i64>,
}

// --- schema v3: the monitor's history, alerts with ids, downloads --------------------------------

/// `playlist_alert` as v3 has it, under `name` (the migration builds it beside the old one).
/// `dedupe_key` decides whether an event is new (see [`alert_dedupe_key`]); `from_pos`/`to_pos`
/// are the positions a `moved` row went between.
fn alert_table_sql(name: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {name} (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            dedupe_key  TEXT UNIQUE,
            playlist_id TEXT NOT NULL,
            video_id    TEXT NOT NULL,
            kind        TEXT NOT NULL
                        CHECK (kind IN ('added', 'removed', 'moved', 'unavailable', 'restored')),
            song_json   TEXT,
            at          INTEGER NOT NULL,
            from_pos    INTEGER,
            to_pos      INTEGER,
            seen        INTEGER NOT NULL DEFAULT 0 CHECK (seen IN (0, 1)),
            dismissed   INTEGER NOT NULL DEFAULT 0 CHECK (dismissed IN (0, 1))
        );"
    )
}

/// Copies the pre-v3 alerts (keyed by playlist, track and kind) into `playlist_alert_v3` and
/// swaps it in. The dedupe key is that same triple, so no alert already filed can come back;
/// `gone` becomes `removed`, and a dismissed row counts as seen.
const LEGACY_ALERTS_COPY: &str = "
    INSERT OR IGNORE INTO playlist_alert_v3(dedupe_key, playlist_id, video_id, kind, song_json,
        at, seen, dismissed)
    SELECT playlist_id || char(31) || video_id || char(31) || k, playlist_id, video_id, k,
        song_json, at, dismissed != 0, dismissed != 0
    FROM (SELECT *, CASE kind WHEN 'gone' THEN 'removed' ELSE kind END AS k FROM playlist_alert)
    WHERE k IN ('added', 'removed', 'moved', 'unavailable', 'restored')
    ORDER BY at, playlist_id, video_id;
    DROP TABLE playlist_alert;
    ALTER TABLE playlist_alert_v3 RENAME TO playlist_alert;";

/// Everything else v3 adds. Epoch seconds, except `downloads`, whose dates are RFC 3339 text as
/// PlaylistForge stores them.
const V3_TABLES: &str = r#"
    CREATE INDEX IF NOT EXISTS playlist_alert_pl ON playlist_alert(playlist_id, at DESC);
    CREATE INDEX IF NOT EXISTS playlist_alert_unseen ON playlist_alert(seen, dismissed);
    -- One playlist's content each time it changed (`put_snapshot_if_changed`). Kept when the
    -- playlist is forgotten or the account signs out: it is the playlist's history.
    CREATE TABLE IF NOT EXISTS playlist_snapshot (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        playlist_id TEXT NOT NULL,
        account_id  TEXT,
        title       TEXT,
        taken_at    INTEGER NOT NULL,
        item_count  INTEGER NOT NULL,
        hash        TEXT NOT NULL,
        items_json  TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS playlist_snapshot_pl
        ON playlist_snapshot(playlist_id, taken_at DESC);
    CREATE TABLE IF NOT EXISTS monitor_runs (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at       INTEGER NOT NULL,
        finished_at      INTEGER NOT NULL,
        "trigger"        TEXT NOT NULL CHECK ("trigger" IN ('manual_ui', 'scheduler', 'headless')),
        outcome          TEXT NOT NULL
                         CHECK (outcome IN ('ok', 'partial', 'failed', 'lock_busy', 'cancelled')),
        playlists_ok     INTEGER NOT NULL DEFAULT 0,
        playlists_failed INTEGER NOT NULL DEFAULT 0,
        alerts_new       INTEGER NOT NULL DEFAULT 0,
        units_spent      INTEGER NOT NULL DEFAULT 0,
        detail_json      TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX IF NOT EXISTS monitor_runs_started ON monitor_runs(started_at DESC);
    -- The last complete sync of each account playlist and what it found.
    CREATE TABLE IF NOT EXISTS playlist_sync (
        playlist_id TEXT PRIMARY KEY,
        synced_at   INTEGER NOT NULL,
        item_count  INTEGER NOT NULL,
        added       INTEGER NOT NULL DEFAULT 0,
        removed     INTEGER NOT NULL DEFAULT 0,
        moved       INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS videos (
        video_id   TEXT PRIMARY KEY,
        title      TEXT,
        channel    TEXT,
        duration_s INTEGER,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS downloads (
        video_id          TEXT NOT NULL REFERENCES videos(video_id),
        format            TEXT NOT NULL CHECK (format IN ('audio', 'video')),
        status            TEXT NOT NULL
                          CHECK (status IN ('queued', 'running', 'available', 'error', 'missing')),
        requested_quality TEXT NOT NULL,
        thumbnail_mode    TEXT NOT NULL DEFAULT 'embed',
        dest_dir          TEXT NOT NULL,
        file_path         TEXT,
        file_size_bytes   INTEGER,
        container         TEXT,
        error             TEXT,
        attempts          INTEGER NOT NULL DEFAULT 0,
        created_at        TEXT NOT NULL,
        completed_at      TEXT,
        last_verified_at  TEXT,
        PRIMARY KEY (video_id, format)
    );
    CREATE INDEX IF NOT EXISTS idx_downloads_status ON downloads(status);
"#;

/// Schema v3, in one transaction: a failure leaves the file exactly as v2 had it, and the next
/// launch tries again. Every step looks before it acts, so running it twice changes nothing.
fn migrate_v3(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // NULL = in the playlist since before anything kept track.
    if !has_column(&tx, "playlist_track", "first_seen")? {
        tx.execute("ALTER TABLE playlist_track ADD COLUMN first_seen INTEGER", [])?;
    }
    let legacy_alerts = has_column(&tx, "playlist_alert", "kind")?
        && !(has_column(&tx, "playlist_alert", "dedupe_key")?
            && has_column(&tx, "playlist_alert", "seen")?);
    if legacy_alerts {
        let rebuild = format!("{}{LEGACY_ALERTS_COPY}", alert_table_sql("playlist_alert_v3"));
        tx.execute_batch(&rebuild)?;
    }
    tx.execute_batch(&alert_table_sql("playlist_alert"))?;
    tx.execute_batch(V3_TABLES)?;
    // Only ever raised: a file some later schema already numbered past 3 keeps its number.
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 3 {
        tx.execute_batch("PRAGMA user_version = 3")?;
    }
    tx.commit()
}

/// Whether every table and column v3 adds is there. Cheap (a handful of catalog lookups), so it
/// runs on every open; any lookup error counts as "not complete" and lets `migrate_v3` decide.
fn v3_complete(conn: &Connection) -> bool {
    let tables = ["playlist_snapshot", "monitor_runs", "playlist_sync", "videos", "downloads"];
    let columns = [
        ("playlist_alert", "dedupe_key"),
        ("playlist_alert", "seen"),
        ("playlist_track", "first_seen"),
    ];
    tables.iter().all(|t| has_table(conn, t))
        && columns.iter().all(|(t, c)| has_column(conn, t, c).unwrap_or(false))
}

/// The tables v4 adds. Dates are RFC 3339 text in UTC, as PlaylistForge stores them, so the quota
/// day is cut by comparing strings and PlaylistForge's ledger imports as is; `added_at` on the
/// accounts is epoch seconds like every other account column in this file.
///
/// Foreign keys are on (`Db::open`), and each one says what a delete does:
/// - a job's items go with it (`ON DELETE CASCADE`; `jobs::repo::delete_job` deletes them by hand
///   as well, so nothing depends on the pragma alone);
/// - disconnecting an account keeps its jobs, unowned (`ON DELETE SET NULL`);
/// - the ledger has no key to the account at all: the day's spend is the Google project's, and
///   it has to survive the account that spent it. Its job link is cleared with the job.
const V4_TABLES: &str = r#"
    CREATE TABLE IF NOT EXISTS ytdata_accounts (
        channel_id     TEXT PRIMARY KEY,
        title          TEXT NOT NULL,
        thumb          TEXT,
        status         TEXT NOT NULL DEFAULT 'connected'
                       CHECK (status IN ('connected', 'reauth_required')),
        added_at       INTEGER NOT NULL,
        linked_account TEXT
    );
    CREATE TABLE IF NOT EXISTS jobs (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        account_id      TEXT REFERENCES ytdata_accounts(channel_id) ON DELETE SET NULL,
        kind            TEXT NOT NULL,
        params_json     TEXT NOT NULL DEFAULT '{}',
        status          TEXT NOT NULL DEFAULT 'queued',
        priority        INTEGER NOT NULL DEFAULT 2,
        phase           INTEGER NOT NULL DEFAULT 1,
        total_phases    INTEGER NOT NULL DEFAULT 1,
        resume_at       TEXT,
        created_at      TEXT NOT NULL,
        started_at      TEXT,
        finished_at     TEXT,
        est_units_total INTEGER NOT NULL DEFAULT 0,
        spent_units     INTEGER NOT NULL DEFAULT 0,
        total_items     INTEGER NOT NULL DEFAULT 0,
        done_items      INTEGER NOT NULL DEFAULT 0,
        failed_items    INTEGER NOT NULL DEFAULT 0,
        skipped_items   INTEGER NOT NULL DEFAULT 0,
        last_error      TEXT,
        planned_items   INTEGER NOT NULL DEFAULT 0,
        retried_items   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_jobs_status_priority_created
        ON jobs(status, priority, created_at);
    CREATE INDEX IF NOT EXISTS idx_jobs_account ON jobs(account_id);
    CREATE TABLE IF NOT EXISTS job_items (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id          INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
        seq             INTEGER NOT NULL,
        phase           INTEGER NOT NULL DEFAULT 1,
        action          TEXT NOT NULL,
        params_json     TEXT NOT NULL DEFAULT '{}',
        status          TEXT NOT NULL DEFAULT 'pending',
        api_result_json TEXT,
        inverse_json    TEXT,
        attempts        INTEGER NOT NULL DEFAULT 0,
        last_error      TEXT,
        updated_at      TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_job_items_job_phase_seq ON job_items(job_id, phase, seq);
    CREATE TABLE IF NOT EXISTS quota_ledger (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        ts         TEXT NOT NULL,
        endpoint   TEXT NOT NULL,
        units      INTEGER NOT NULL,
        account_id TEXT,
        job_id     INTEGER REFERENCES jobs(id) ON DELETE SET NULL
    );
    CREATE INDEX IF NOT EXISTS idx_quota_ledger_ts ON quota_ledger(ts);
    CREATE INDEX IF NOT EXISTS idx_quota_ledger_job ON quota_ledger(job_id);
    -- One row at most: the process running jobs right now, and when it last said so.
    CREATE TABLE IF NOT EXISTS runner_lock (
        id           INTEGER PRIMARY KEY CHECK (id = 1),
        pid          INTEGER NOT NULL,
        heartbeat_at TEXT NOT NULL
    );
"#;

/// Schema v4, in one transaction, like [`migrate_v3`]: new tables and two added columns, nothing
/// rebuilt, so the foreign-key pragma can stay on throughout.
fn migrate_v4(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // When the track was added to the playlist, epoch seconds, as the Data API reports it.
    // NULL = unknown (InnerTube does not say, and rows indexed before v4 never had it).
    if !has_column(&tx, "playlist_track", "added_at")? {
        tx.execute("ALTER TABLE playlist_track ADD COLUMN added_at INTEGER", [])?;
    }
    // The playlist's privacy at its last sync. NULL = not known yet.
    if !has_column(&tx, "playlist_sync", "privacy")? {
        tx.execute(
            "ALTER TABLE playlist_sync ADD COLUMN privacy TEXT \
             CHECK (privacy IN ('public', 'unlisted', 'private'))",
            [],
        )?;
    }
    tx.execute_batch(V4_TABLES)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 4 {
        tx.execute_batch("PRAGMA user_version = 4")?;
    }
    tx.commit()
}

/// Whether every table and column v4 adds is there. Same contract as [`v3_complete`].
fn v4_complete(conn: &Connection) -> bool {
    let tables = ["ytdata_accounts", "jobs", "job_items", "quota_ledger", "runner_lock"];
    let columns = [("playlist_track", "added_at"), ("playlist_sync", "privacy")];
    tables.iter().all(|t| has_table(conn, t))
        && columns.iter().all(|(t, c)| has_column(conn, t, c).unwrap_or(false))
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .is_ok_and(|n| n > 0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

/// Rewrite one playlist's index rows, keeping each surviving track's `first_seen`. A track new
/// to the playlist gets `now`, but only when the playlist has synced before (it has a
/// `playlist_sync` row): on its first read nobody knows when its tracks arrived.
fn replace_playlist_rows(
    conn: &Connection,
    playlist_id: &str,
    rows: &[(&str, Option<&str>)],
    now: i64,
) -> rusqlite::Result<()> {
    let synced: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM playlist_sync WHERE playlist_id = ?1)",
        [playlist_id],
        |r| r.get(0),
    )?;
    // A surviving track also keeps its v4 `added_at`: only the Data API knows it, and an
    // InnerTube rewrite must not wipe what the last Data API sync stored.
    let known: std::collections::HashMap<String, (Option<i64>, Option<i64>)> = {
        let mut stmt = conn.prepare(
            "SELECT video_id, first_seen, added_at FROM playlist_track WHERE playlist_id = ?1",
        )?;
        let found = stmt
            .query_map([playlist_id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;
        found
    };
    conn.execute("DELETE FROM playlist_track WHERE playlist_id = ?1", [playlist_id])?;
    let fresh = synced.then_some(now);
    for (video_id, json) in rows {
        let (first_seen, added_at) = known.get(*video_id).copied().unwrap_or((fresh, None));
        conn.execute(
            "INSERT OR IGNORE INTO playlist_track(playlist_id, video_id, song_json, first_seen, \
             added_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![playlist_id, video_id, json, first_seen, added_at],
        )?;
    }
    Ok(())
}

/// The kind as v3 files it. The pre-v3 `gone` is `removed`; any other unknown kind is refused by
/// the table's CHECK.
fn alert_kind(kind: &str) -> &str {
    match kind {
        "gone" => "removed",
        other => other,
    }
}

/// An alert's dedupe key: playlist, track and kind, joined by U+001F, plus a scope (a snapshot or
/// run id) for an event that can happen again and should then file a row of its own. With no
/// scope it is the pre-v3 identity, one alert per playlist, track and kind ever, which is the
/// key the migration gave every existing row.
pub fn alert_dedupe_key(
    playlist_id: &str,
    video_id: &str,
    kind: &str,
    scope: Option<&str>,
) -> String {
    let mut key = format!("{playlist_id}\u{1f}{video_id}\u{1f}{kind}");
    if let Some(scope) = scope {
        key.push('\u{1f}');
        key.push_str(scope);
    }
    key
}

/// One alert to file.
#[derive(Debug, Clone)]
pub struct NewAlert<'a> {
    pub playlist_id: &'a str,
    pub video_id: &'a str,
    pub kind: &'a str,
    pub song_json: Option<&'a str>,
    pub at: i64,
    /// The positions a `moved` row went between (0-based), where known.
    pub from_pos: Option<i64>,
    pub to_pos: Option<i64>,
    pub dedupe_key: &'a str,
}

/// One filed alert, flags included.
#[allow(dead_code)] // read by the alerts page (commit 15)
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AlertRow {
    pub id: i64,
    pub playlist_id: String,
    pub video_id: String,
    pub kind: String,
    pub song_json: Option<String>,
    pub at: i64,
    pub from_pos: Option<i64>,
    pub to_pos: Option<i64>,
    pub seen: bool,
    pub dismissed: bool,
    /// An `unavailable` row a later `restored` of the same playlist and track answers. Never
    /// true for any other kind.
    pub resolved: bool,
}

/// One playlist row as a snapshot keeps it, compact because a snapshot is taken on every change
/// of every playlist: `v` videoId, `s` setVideoId, `t` title, `a` artists, `d` duration as
/// YouTube writes it, `u` unavailable, `th` thumbnail.
#[allow(dead_code)] // built by the monitor (commit 13)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapItem {
    pub v: String,
    #[serde(default)]
    pub s: Option<String>,
    pub t: String,
    #[serde(default)]
    pub a: String,
    #[serde(default)]
    pub d: Option<String>,
    #[serde(default)]
    pub u: bool,
    #[serde(default)]
    pub th: Option<String>,
}

#[allow(dead_code)] // the monitor (commit 13)
impl SnapItem {
    pub fn from_song(song: &innertube::SongItem) -> Self {
        SnapItem {
            v: song.video_id.clone(),
            s: song.set_video_id.clone(),
            t: song.title.clone(),
            a: song.artists.clone(),
            d: song.duration.clone(),
            u: song.unavailable,
            th: song.thumbnail.clone(),
        }
    }
}

/// A snapshot's content hash: MD5 hex over each item's `v`, `s`, `t` and `u`, fields joined by
/// U+001F and items ended by U+001E. MD5 for the reason [`account_key`] gives: a stored value
/// must not change with the toolchain, which `DefaultHasher` does not promise. Order counts, so
/// a move is a change; artists, duration and artwork do not.
#[allow(dead_code)]
pub fn snapshot_hash(items: &[SnapItem]) -> String {
    let mut digest = Md5::new();
    for item in items {
        digest.update(item.v.as_bytes());
        digest.update(b"\x1f");
        digest.update(item.s.as_deref().unwrap_or("").as_bytes());
        digest.update(b"\x1f");
        digest.update(item.t.as_bytes());
        digest.update(b"\x1f");
        digest.update(if item.u { b"1" } else { b"0" });
        digest.update(b"\x1e");
    }
    format!("{:x}", digest.finalize())
}

/// One stored snapshot, newest-first in every list.
#[allow(dead_code)] // the monitor, backups and timeline (commits 13-15)
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub id: i64,
    pub playlist_id: String,
    pub account_id: Option<String>,
    pub title: Option<String>,
    pub taken_at: i64,
    pub item_count: i64,
    pub hash: String,
    pub items: Vec<SnapItem>,
}

#[allow(dead_code)]
const SNAPSHOT_SELECT: &str = "SELECT id, playlist_id, account_id, title, taken_at, item_count, \
     hash, items_json FROM playlist_snapshot";
#[allow(dead_code)]
const SNAPSHOT_ORDER: &str = "ORDER BY taken_at DESC, id DESC";

#[allow(dead_code)]
fn snapshot_row(r: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
    let items_json: String = r.get(7)?;
    Ok(Snapshot {
        id: r.get(0)?,
        playlist_id: r.get(1)?,
        account_id: r.get(2)?,
        title: r.get(3)?,
        taken_at: r.get(4)?,
        item_count: r.get(5)?,
        hash: r.get(6)?,
        items: serde_json::from_str(&items_json).unwrap_or_default(),
    })
}

#[allow(dead_code)]
fn prune_snapshots_in(conn: &Connection, keep: i64) -> rusqlite::Result<Vec<i64>> {
    let doomed: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY playlist_id \
             ORDER BY taken_at DESC, id DESC) AS n FROM playlist_snapshot) WHERE n > ?1",
        )?;
        let ids = stmt.query_map([keep], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids
    };
    for id in &doomed {
        conn.execute("DELETE FROM playlist_snapshot WHERE id = ?1", [id])?;
    }
    Ok(doomed)
}

/// One monitor run. `trigger` is `manual_ui`, `scheduler` or `headless`; `outcome` is `ok`,
/// `partial`, `failed`, `lock_busy` or `cancelled` (the table refuses anything else).
#[allow(dead_code)] // the monitor and its page (commits 13, 16)
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MonitorRun {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub trigger: String,
    pub outcome: String,
    pub playlists_ok: i64,
    pub playlists_failed: i64,
    pub alerts_new: i64,
    pub units_spent: i64,
    pub detail_json: String,
}

/// The monitor page's numbers ([`Db::monitor_stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct MonitorStats {
    pub playlists: i64,
    pub items: i64,
    pub unavailable: i64,
    pub duplicates_estimate: i64,
}

/// When an alert was filed and of what kind: one bar segment of the monitor page's chart.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AlertStamp {
    pub at: i64,
    pub kind: String,
}

/// One account playlist's last complete sync and what it found.
#[allow(dead_code)] // the monitor and the library (commits 13, 17)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PlaylistSync {
    pub synced_at: i64,
    pub item_count: i64,
    pub added: i64,
    pub removed: i64,
    pub moved: i64,
    /// The privacy the last Data API sync read (InnerTube does not say). `None` writes nothing:
    /// a later InnerTube sync keeps what the Data API stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privacy: Option<Privacy>,
}

/// A playlist's `status.privacyStatus`, as `playlist_sync.privacy` stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    Public,
    Unlisted,
    Private,
}

impl Privacy {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "public" => Some(Privacy::Public),
            "unlisted" => Some(Privacy::Unlisted),
            "private" => Some(Privacy::Private),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Privacy::Public => "public",
            Privacy::Unlisted => "unlisted",
            Privacy::Private => "private",
        }
    }
}

/// What the `videos` table knows about a video. `None` leaves a known value as it is.
#[derive(Debug, Clone, Copy)]
pub struct VideoMeta<'a> {
    pub video_id: &'a str,
    pub title: Option<&'a str>,
    pub channel: Option<&'a str>,
    pub duration_s: Option<i64>,
}

/// A download to queue. `format` is `audio` or `video` (the table refuses anything else).
#[derive(Debug, Clone, Copy)]
pub struct NewDownload<'a> {
    pub video: VideoMeta<'a>,
    pub format: &'a str,
    pub requested_quality: &'a str,
    pub thumbnail_mode: &'a str,
    pub dest_dir: &'a str,
    /// Queue it again even when it is already downloaded.
    pub redownload: bool,
}

/// What [`Db::enqueue_download`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new row.
    Queued,
    /// An errored, missing or (with `redownload`) available row, queued again.
    Requeued,
    /// Already queued, running or available: nothing changed.
    Already,
}

/// One `downloads` row. The dates are RFC 3339 UTC text ([`rfc3339_utc`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DownloadRow {
    pub video_id: String,
    pub format: String,
    pub status: String,
    pub requested_quality: String,
    pub thumbnail_mode: String,
    pub dest_dir: String,
    pub file_path: Option<String>,
    pub file_size_bytes: Option<i64>,
    pub container: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub last_verified_at: Option<String>,
}

const DOWNLOAD_SELECT: &str = "SELECT video_id, format, status, requested_quality, \
     thumbnail_mode, dest_dir, file_path, file_size_bytes, container, error, attempts, \
     created_at, completed_at, last_verified_at FROM downloads";

fn download_row(r: &rusqlite::Row) -> rusqlite::Result<DownloadRow> {
    Ok(DownloadRow {
        video_id: r.get(0)?,
        format: r.get(1)?,
        status: r.get(2)?,
        requested_quality: r.get(3)?,
        thumbnail_mode: r.get(4)?,
        dest_dir: r.get(5)?,
        file_path: r.get(6)?,
        file_size_bytes: r.get(7)?,
        container: r.get(8)?,
        error: r.get(9)?,
        attempts: r.get(10)?,
        created_at: r.get(11)?,
        completed_at: r.get(12)?,
        last_verified_at: r.get(13)?,
    })
}

fn upsert_video_in(conn: &Connection, video: &VideoMeta<'_>, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO videos(video_id, title, channel, duration_s, updated_at) \
         VALUES(?1, ?2, ?3, ?4, ?5) ON CONFLICT(video_id) DO UPDATE SET \
         title = COALESCE(excluded.title, title), channel = COALESCE(excluded.channel, channel), \
         duration_s = COALESCE(excluded.duration_s, duration_s), updated_at = excluded.updated_at",
        rusqlite::params![video.video_id, video.title, video.channel, video.duration_s, now],
    )?;
    Ok(())
}

/// The v4 tables' date form: [`rfc3339_utc`] of a chrono instant, so every stored date has one
/// fixed width and the quota day can be cut by comparing text. Sub-second precision is dropped.
pub(crate) fn rfc3339_text(at: chrono::DateTime<chrono::Utc>) -> String {
    rfc3339_utc(at.timestamp())
}

/// Reads a stored RFC 3339 date in any offset (PlaylistForge writes `+00:00`). An unreadable
/// value reads as the epoch rather than failing the whole row: it is a display date, never a key.
pub(crate) fn parse_rfc3339(raw: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
}

/// Unix seconds as RFC 3339 UTC (`2023-11-14T22:13:20Z`), the form `downloads` stores its dates
/// in. Sorts as text in time order. Days to civil date after Howard Hinnant's `civil_from_days`.
pub fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// What is stored about some videos' titles, for the recovery assistant
/// ([`Db::local_title_rows`]), each list newest first where its source has an order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalTitleRows {
    /// `(videoId, title, artists)` of the snapshot rows that still played when taken.
    pub snapshots: Vec<(String, String, String)>,
    /// `(videoId, song_json)` of alerts.
    pub alerts: Vec<(String, String)>,
    /// `(videoId, song_json)` of the play history.
    pub plays: Vec<(String, String)>,
    /// `(videoId, title, channel)` from `videos` (downloads, the PlaylistForge import).
    pub videos: Vec<(String, String, Option<String>)>,
    /// `(videoId, song_json)` of the membership index.
    pub index: Vec<(String, String)>,
}

/// The rows of `sql`, whose only parameter (`?1`) is `wanted`. A failed query is no rows.
fn wanted_rows<T>(
    conn: &Connection,
    sql: &str,
    wanted: &str,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Vec<T> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(sql) {
        if let Ok(rows) = stmt.query_map([wanted], f) {
            out.extend(rows.flatten());
        }
    }
    out
}

fn id_and_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String)> {
    Ok((r.get(0)?, r.get(1)?))
}

/// The columns an [`AlertRow`] is read from, over `playlist_alert a`. The last one is `resolved`:
/// an `unavailable` row a `restored` of the same playlist and track filed after it answers. Only
/// a later one: a track that came back and then went again is open once more. Ties on `at` fall
/// to the id, the order they were filed in. `playlist_alert_pvk` keeps the lookup cheap.
const ALERT_ROW_COLUMNS: &str = "a.id, a.playlist_id, a.video_id, a.kind, a.song_json, a.at, \
    a.from_pos, a.to_pos, a.seen, a.dismissed, \
    a.kind = 'unavailable' AND EXISTS(SELECT 1 FROM playlist_alert r \
    WHERE r.playlist_id = a.playlist_id AND r.video_id = a.video_id AND r.kind = 'restored' \
    AND (r.at > a.at OR (r.at = a.at AND r.id > a.id)))";

#[allow(dead_code)] // the monitor, backups, alerts and downloads (commits 13-21)
impl Db {
    // --- alerts (v3) --------------------------------------------------------------------------

    /// File an alert unless one with its dedupe key is already there (dismissed ones included).
    /// Answers the new row's id; `None` for a repeat, or a kind the table refuses.
    pub fn insert_alert(&self, alert: &NewAlert<'_>) -> Option<i64> {
        let conn = self.0.lock().unwrap();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO playlist_alert(dedupe_key, playlist_id, video_id, kind, \
             song_json, at, from_pos, to_pos) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                alert.dedupe_key,
                alert.playlist_id,
                alert.video_id,
                alert.kind,
                alert.song_json,
                alert.at,
                alert.from_pos,
                alert.to_pos
            ],
        );
        match inserted {
            Ok(n) if n > 0 => Some(conn.last_insert_rowid()),
            _ => None,
        }
    }

    /// Alerts newest first, with their ids and flags. Dismissed ones only when asked for.
    pub fn alert_rows(&self, include_dismissed: bool) -> Vec<AlertRow> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "SELECT {ALERT_ROW_COLUMNS} FROM playlist_alert a {} ORDER BY a.at DESC, a.id DESC",
            if include_dismissed { "" } else { "WHERE a.dismissed = 0" }
        );
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(AlertRow {
                    id: r.get(0)?,
                    playlist_id: r.get(1)?,
                    video_id: r.get(2)?,
                    kind: r.get(3)?,
                    song_json: r.get(4)?,
                    at: r.get(5)?,
                    from_pos: r.get(6)?,
                    to_pos: r.get(7)?,
                    seen: r.get::<_, i64>(8)? != 0,
                    dismissed: r.get::<_, i64>(9)? != 0,
                    resolved: r.get::<_, i64>(10)? != 0,
                })
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// One page of [`Db::alert_rows`], newest first: at most `limit` rows (all with `None`), and
    /// only those after the `before` cursor, the `(at, id)` of the last row of the page before.
    /// Ordering by both means rows that share an `at` are neither skipped nor repeated.
    pub fn alert_rows_page(
        &self,
        include_dismissed: bool,
        limit: Option<u32>,
        before: Option<(i64, i64)>,
    ) -> Vec<AlertRow> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {ALERT_ROW_COLUMNS} FROM playlist_alert a WHERE (?1 OR a.dismissed = 0) \
             AND (?2 IS NULL OR a.at < ?2 OR (a.at = ?2 AND a.id < ?3)) \
             ORDER BY a.at DESC, a.id DESC LIMIT ?4"
        )) {
            let (at, id) = (before.map(|b| b.0), before.map(|b| b.1));
            let limit = limit.map_or(-1, i64::from);
            let params = rusqlite::params![include_dismissed, at, id, limit];
            if let Ok(rows) = stmt.query_map(params, |r| {
                Ok(AlertRow {
                    id: r.get(0)?,
                    playlist_id: r.get(1)?,
                    video_id: r.get(2)?,
                    kind: r.get(3)?,
                    song_json: r.get(4)?,
                    at: r.get(5)?,
                    from_pos: r.get(6)?,
                    to_pos: r.get(7)?,
                    seen: r.get::<_, i64>(8)? != 0,
                    dismissed: r.get::<_, i64>(9)? != 0,
                    resolved: r.get::<_, i64>(10)? != 0,
                })
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Mark these alerts seen, or every alert with `None`. Answers how many rows it touched.
    pub fn mark_alerts_seen(&self, ids: Option<&[i64]>) -> usize {
        let conn = self.0.lock().unwrap();
        let done = match ids {
            None => conn.execute("UPDATE playlist_alert SET seen = 1 WHERE seen = 0", []),
            Some([]) => Ok(0),
            Some(ids) => {
                let holes = vec!["?"; ids.len()].join(",");
                conn.execute(
                    &format!("UPDATE playlist_alert SET seen = 1 WHERE id IN ({holes})"),
                    rusqlite::params_from_iter(ids.iter()),
                )
            }
        };
        done.unwrap_or(0)
    }

    /// Alerts neither seen nor dismissed: the sidebar badge.
    pub fn unseen_alert_count(&self) -> u32 {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM playlist_alert WHERE seen = 0 AND dismissed = 0",
            [],
            |r| r.get::<_, u32>(0),
        )
        .unwrap_or(0)
    }

    // --- snapshots ----------------------------------------------------------------------------

    /// Store a snapshot of a playlist unless its newest one holds the same content
    /// ([`snapshot_hash`]: a new title alone is no change). Answers the new snapshot's id, or
    /// `None` when nothing changed (or the write failed).
    pub fn put_snapshot_if_changed(
        &self,
        playlist_id: &str,
        account_id: Option<&str>,
        title: Option<&str>,
        at: i64,
        items: &[SnapItem],
    ) -> Option<i64> {
        let hash = snapshot_hash(items);
        let items_json = serde_json::to_string(items).ok()?;
        let conn = self.0.lock().unwrap();
        let latest: Option<String> = conn
            .query_row(
                "SELECT hash FROM playlist_snapshot WHERE playlist_id = ?1 \
                 ORDER BY taken_at DESC, id DESC LIMIT 1",
                [playlist_id],
                |r| r.get(0),
            )
            .ok();
        if latest.as_deref() == Some(hash.as_str()) {
            return None;
        }
        conn.execute(
            "INSERT INTO playlist_snapshot(playlist_id, account_id, title, taken_at, item_count, \
             hash, items_json) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                playlist_id,
                account_id,
                title,
                at,
                items.len() as i64,
                hash,
                items_json
            ],
        )
        .ok()?;
        Some(conn.last_insert_rowid())
    }

    /// A playlist's newest snapshot: the "before" its next sync is compared against.
    pub fn latest_snapshot(&self, playlist_id: &str) -> Option<Snapshot> {
        let conn = self.0.lock().unwrap();
        let sql = format!("{SNAPSHOT_SELECT} WHERE playlist_id = ?1 {SNAPSHOT_ORDER} LIMIT 1");
        conn.query_row(&sql, [playlist_id], snapshot_row).ok()
    }

    /// Every snapshot of a playlist, newest first.
    pub fn snapshots(&self, playlist_id: &str) -> Vec<Snapshot> {
        let conn = self.0.lock().unwrap();
        let sql = format!("{SNAPSHOT_SELECT} WHERE playlist_id = ?1 {SNAPSHOT_ORDER}");
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([playlist_id], snapshot_row) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    pub fn snapshot(&self, id: i64) -> Option<Snapshot> {
        let conn = self.0.lock().unwrap();
        conn.query_row(&format!("{SNAPSHOT_SELECT} WHERE id = ?1"), [id], snapshot_row).ok()
    }

    /// Keep each playlist's newest `keep` snapshots and delete the rest, answering the deleted
    /// ids. Never fewer than two per playlist: the current one (the next sync's "before") and the
    /// one before it (the reference its latest changes were computed against) always stay.
    pub fn prune_snapshots(&self, keep: usize) -> Vec<i64> {
        let mut conn = self.0.lock().unwrap();
        let Ok(tx) = conn.transaction() else { return Vec::new() };
        let Ok(gone) = prune_snapshots_in(&tx, keep.max(2) as i64) else { return Vec::new() };
        if tx.commit().is_err() {
            return Vec::new();
        }
        gone
    }

    /// Every playlist with at least one snapshot, sync record or not: a forgotten playlist's
    /// history, or one kept from before a sign-out, still gets backed up.
    pub fn snapshot_playlist_ids(&self) -> Vec<String> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT DISTINCT playlist_id FROM playlist_snapshot ORDER BY playlist_id")
        {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    // --- the recovery assistant (playlist_tools::recover) --------------------------------------

    /// Each playlist's newest snapshot, by playlist id, in one query.
    pub fn latest_snapshots(&self) -> std::collections::HashMap<String, Snapshot> {
        let conn = self.0.lock().unwrap();
        let sql = "SELECT id, playlist_id, account_id, title, taken_at, item_count, hash, \
             items_json FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY playlist_id \
             ORDER BY taken_at DESC, id DESC) AS n FROM playlist_snapshot) WHERE n = 1";
        let mut out = std::collections::HashMap::new();
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(rows) = stmt.query_map([], snapshot_row) {
                out.extend(rows.flatten().map(|s| (s.playlist_id.clone(), s)));
            }
        }
        out
    }

    /// A playlist's newest snapshot taken strictly before `at`: the "before" an alert filed at `at`
    /// was found against (the monitor files it at the time of the read that took the next one).
    pub fn snapshot_before(&self, playlist_id: &str, at: i64) -> Option<Snapshot> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "{SNAPSHOT_SELECT} WHERE playlist_id = ?1 AND taken_at < ?2 {SNAPSHOT_ORDER} LIMIT 1"
        );
        conn.query_row(&sql, rusqlite::params![playlist_id, at], snapshot_row).ok()
    }

    /// `(playlist id, videoId, song_json)` of every index row whose song is marked unavailable.
    pub fn unavailable_index_rows(&self) -> Vec<(String, String, String)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT playlist_id, video_id, song_json FROM playlist_track \
             WHERE json_extract(song_json, '$.unavailable') = 1",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Everything stored that names one of `video_ids`, for
    /// [`best_local_titles`](crate::playlist_tools::recover::best_local_titles): one query per
    /// source under one lock, the ids passed as a single JSON array.
    pub fn local_title_rows(&self, video_ids: &[String]) -> LocalTitleRows {
        let Ok(wanted) = serde_json::to_string(video_ids) else { return LocalTitleRows::default() };
        let conn = self.0.lock().unwrap();
        let pair = id_and_json;
        LocalTitleRows {
            snapshots: wanted_rows(
                &conn,
                "SELECT json_extract(j.value, '$.v'), json_extract(j.value, '$.t'), \
                 json_extract(j.value, '$.a') \
                 FROM playlist_snapshot s, json_each(s.items_json) j \
                 WHERE json_extract(j.value, '$.v') IN (SELECT value FROM json_each(?1)) \
                 AND json_extract(j.value, '$.u') IS NOT 1 \
                 ORDER BY s.taken_at DESC, s.id DESC",
                &wanted,
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
                },
            ),
            alerts: wanted_rows(
                &conn,
                "SELECT video_id, song_json FROM playlist_alert WHERE song_json IS NOT NULL \
                 AND video_id IN (SELECT value FROM json_each(?1)) ORDER BY at DESC, id DESC",
                &wanted,
                pair,
            ),
            plays: wanted_rows(
                &conn,
                "SELECT video_id, song_json FROM plays \
                 WHERE video_id IN (SELECT value FROM json_each(?1)) \
                 ORDER BY played_at DESC, id DESC",
                &wanted,
                pair,
            ),
            videos: wanted_rows(
                &conn,
                "SELECT video_id, title, channel FROM videos WHERE title IS NOT NULL \
                 AND video_id IN (SELECT value FROM json_each(?1))",
                &wanted,
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ),
            index: wanted_rows(
                &conn,
                "SELECT video_id, song_json FROM playlist_track WHERE song_json IS NOT NULL \
                 AND video_id IN (SELECT value FROM json_each(?1))",
                &wanted,
                pair,
            ),
        }
    }

    // --- monitor runs and per-playlist sync -----------------------------------------------------

    /// Log one monitor run (its `id` is ignored) and answer the new id.
    pub fn record_monitor_run(&self, run: &MonitorRun) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO monitor_runs(started_at, finished_at, \"trigger\", outcome, \
             playlists_ok, playlists_failed, alerts_new, units_spent, detail_json) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                run.started_at,
                run.finished_at,
                run.trigger,
                run.outcome,
                run.playlists_ok,
                run.playlists_failed,
                run.alerts_new,
                run.units_spent,
                run.detail_json
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Fold a repeat into an existing run row: a failure just like the one before it moves that
    /// row's `finished_at` and replaces its `detail_json` (which counts the repeats) instead of
    /// logging a row of its own.
    pub fn update_monitor_run_repeat(
        &self,
        id: i64,
        finished_at: i64,
        detail_json: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE monitor_runs SET finished_at = ?2, detail_json = ?3 WHERE id = ?1",
            rusqlite::params![id, finished_at, detail_json],
        )?;
        Ok(())
    }

    /// The newest `limit` monitor runs, newest first.
    pub fn monitor_runs(&self, limit: usize) -> Vec<MonitorRun> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, started_at, finished_at, \"trigger\", outcome, playlists_ok, \
             playlists_failed, alerts_new, units_spent, detail_json FROM monitor_runs \
             ORDER BY started_at DESC, id DESC LIMIT ?1",
        ) {
            if let Ok(rows) = stmt.query_map([limit as i64], |r| {
                Ok(MonitorRun {
                    id: r.get(0)?,
                    started_at: r.get(1)?,
                    finished_at: r.get(2)?,
                    trigger: r.get(3)?,
                    outcome: r.get(4)?,
                    playlists_ok: r.get(5)?,
                    playlists_failed: r.get(6)?,
                    alerts_new: r.get(7)?,
                    units_spent: r.get(8)?,
                    detail_json: r.get(9)?,
                })
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// The monitor page's cards: playlists in the index, the tracks in them, how many of those
    /// YouTube greys out, and an estimate of the extra copies. The playlists are the ones with
    /// index rows or a sync record (an empty one has only the latter); `items` counts index rows,
    /// one per track and playlist. The duplicates are PlaylistForge's `library_stats` estimate:
    /// within each synced playlist's newest snapshot (the index keeps a track once per playlist,
    /// a snapshot keeps every copy), every copy of a video past its first.
    pub fn monitor_stats(&self) -> MonitorStats {
        let conn = self.0.lock().unwrap();
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        MonitorStats {
            playlists: count(
                "SELECT COUNT(*) FROM (SELECT playlist_id FROM playlist_track \
                 UNION SELECT playlist_id FROM playlist_sync)",
            ),
            items: count("SELECT COUNT(*) FROM playlist_track"),
            unavailable: count(
                "SELECT COUNT(*) FROM playlist_track \
                 WHERE json_extract(song_json, '$.unavailable') = 1",
            ),
            duplicates_estimate: count(
                "WITH cur AS (SELECT s.playlist_id, s.items_json FROM playlist_snapshot s \
                     WHERE s.playlist_id IN (SELECT playlist_id FROM playlist_sync) \
                     AND s.id = (SELECT s2.id FROM playlist_snapshot s2 \
                         WHERE s2.playlist_id = s.playlist_id \
                         ORDER BY s2.taken_at DESC, s2.id DESC LIMIT 1)) \
                 SELECT COALESCE(SUM(cnt - 1), 0) FROM ( \
                     SELECT COUNT(*) AS cnt FROM cur, json_each(cur.items_json) j \
                     GROUP BY cur.playlist_id, json_extract(j.value, '$.v') \
                     HAVING COUNT(*) > 1)",
            ),
        }
    }

    /// Every alert filed at or after `since` (dismissed ones too: they still happened), oldest
    /// first, as `(at, kind)`. The monitor page buckets them by local day for its chart.
    pub fn alerts_since(&self, since: i64) -> Vec<AlertStamp> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT at, kind FROM playlist_alert WHERE at >= ?1 ORDER BY at, id")
        {
            if let Ok(rows) =
                stmt.query_map([since], |r| Ok(AlertStamp { at: r.get(0)?, kind: r.get(1)? }))
            {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Record a playlist's complete sync. Write its index rows first: whether a track is new
    /// enough to get a `first_seen` depends on there being no sync record yet.
    pub fn set_playlist_sync(
        &self,
        playlist_id: &str,
        sync: &PlaylistSync,
    ) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO playlist_sync(playlist_id, synced_at, item_count, added, removed, moved, \
             privacy) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(playlist_id) DO UPDATE SET \
             synced_at = excluded.synced_at, item_count = excluded.item_count, \
             added = excluded.added, removed = excluded.removed, moved = excluded.moved, \
             privacy = COALESCE(excluded.privacy, playlist_sync.privacy)",
            rusqlite::params![
                playlist_id,
                sync.synced_at,
                sync.item_count,
                sync.added,
                sync.removed,
                sync.moved,
                sync.privacy.map(Privacy::as_str)
            ],
        )?;
        Ok(())
    }

    /// Playlist id → its last complete sync.
    pub fn playlist_syncs(&self) -> std::collections::HashMap<String, PlaylistSync> {
        let conn = self.0.lock().unwrap();
        let mut out = std::collections::HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT playlist_id, synced_at, item_count, added, removed, moved, privacy \
             FROM playlist_sync",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                let privacy: Option<String> = r.get(6)?;
                let sync = PlaylistSync {
                    synced_at: r.get(1)?,
                    item_count: r.get(2)?,
                    added: r.get(3)?,
                    removed: r.get(4)?,
                    moved: r.get(5)?,
                    privacy: privacy.as_deref().and_then(Privacy::parse),
                };
                Ok((r.get::<_, String>(0)?, sync))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    // --- videos and downloads -------------------------------------------------------------------

    /// Insert or refresh what is known about a video.
    pub fn upsert_video(&self, video: &VideoMeta<'_>, now: i64) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        upsert_video_in(&conn, video, now)
    }

    /// Queue a download, recording its video first. A new row is `queued`; an `error` or
    /// `missing` row is queued again, and an `available` one too with `redownload`; a row already
    /// `queued` or `running` (or available without `redownload`) is left alone. A requeue takes
    /// the request's quality, thumbnail mode and folder, and goes to the back of the queue.
    pub fn enqueue_download(
        &self,
        req: &NewDownload<'_>,
        now: i64,
    ) -> rusqlite::Result<EnqueueOutcome> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        upsert_video_in(&tx, &req.video, now)?;
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM downloads WHERE video_id = ?1 AND format = ?2",
                [req.video.video_id, req.format],
                |r| r.get(0),
            )
            .optional()?;
        let outcome = match status.as_deref() {
            None => EnqueueOutcome::Queued,
            Some("error" | "missing") => EnqueueOutcome::Requeued,
            Some("available") if req.redownload => EnqueueOutcome::Requeued,
            Some(_) => EnqueueOutcome::Already,
        };
        if outcome != EnqueueOutcome::Already {
            let sql = match outcome {
                EnqueueOutcome::Queued => DOWNLOAD_INSERT,
                _ => DOWNLOAD_REQUEUE,
            };
            tx.execute(
                sql,
                rusqlite::params![
                    req.video.video_id,
                    req.format,
                    req.requested_quality,
                    req.thumbnail_mode,
                    req.dest_dir,
                    rfc3339_utc(now)
                ],
            )?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// The runner picked it up: `queued` → `running`, one more attempt. `false` when it was not
    /// queued (cancelled, removed, or taken already).
    pub fn mark_download_running(&self, video_id: &str, format: &str) -> bool {
        self.update_download(
            "UPDATE downloads SET status = 'running', attempts = attempts + 1, error = NULL \
             WHERE video_id = ?1 AND format = ?2 AND status = 'queued'",
            rusqlite::params![video_id, format],
        )
    }

    /// Downloaded (or found on disk by the verifier): the file, its size and container.
    pub fn mark_download_available(
        &self,
        video_id: &str,
        format: &str,
        file_path: &str,
        file_size_bytes: Option<i64>,
        container: Option<&str>,
        now: i64,
    ) -> bool {
        let stamp = rfc3339_utc(now);
        self.update_download(
            "UPDATE downloads SET status = 'available', file_path = ?3, file_size_bytes = ?4, \
             container = ?5, error = NULL, completed_at = ?6, last_verified_at = ?6 \
             WHERE video_id = ?1 AND format = ?2",
            rusqlite::params![video_id, format, file_path, file_size_bytes, container, stamp],
        )
    }

    pub fn mark_download_error(&self, video_id: &str, format: &str, error: &str) -> bool {
        self.update_download(
            "UPDATE downloads SET status = 'error', error = ?3 \
             WHERE video_id = ?1 AND format = ?2",
            rusqlite::params![video_id, format, error],
        )
    }

    /// An `available` download whose file is gone from disk.
    pub fn mark_download_missing(&self, video_id: &str, format: &str, now: i64) -> bool {
        let stamp = rfc3339_utc(now);
        self.update_download(
            "UPDATE downloads SET status = 'missing', last_verified_at = ?3 \
             WHERE video_id = ?1 AND format = ?2 AND status = 'available'",
            rusqlite::params![video_id, format, stamp],
        )
    }

    /// An `available` download whose file is still there.
    pub fn mark_download_verified(&self, video_id: &str, format: &str, now: i64) -> bool {
        let stamp = rfc3339_utc(now);
        self.update_download(
            "UPDATE downloads SET last_verified_at = ?3 \
             WHERE video_id = ?1 AND format = ?2 AND status = 'available'",
            rusqlite::params![video_id, format, stamp],
        )
    }

    /// The verifier found the file of an `available` or `missing` row, at its recorded path or
    /// relocated by its `[id]` marker: `available` again, with that path and size. `container`
    /// `None` keeps the recorded one. Unlike [`Self::mark_download_available`] the completion
    /// date stays: nothing was downloaded.
    pub fn mark_download_found(
        &self,
        video_id: &str,
        format: &str,
        file_path: &str,
        file_size_bytes: Option<i64>,
        container: Option<&str>,
        now: i64,
    ) -> bool {
        let stamp = rfc3339_utc(now);
        self.update_download(
            "UPDATE downloads SET status = 'available', file_path = ?3, \
             file_size_bytes = COALESCE(?4, file_size_bytes), \
             container = COALESCE(?5, container), last_verified_at = ?6 \
             WHERE video_id = ?1 AND format = ?2 AND status IN ('available', 'missing')",
            rusqlite::params![video_id, format, file_path, file_size_bytes, container, stamp],
        )
    }

    /// The videos with an `available` or `missing` download: what the startup check looks at.
    pub fn verifiable_download_ids(&self) -> Vec<String> {
        let conn = self.0.lock().unwrap();
        let sql = "SELECT DISTINCT video_id FROM downloads \
                   WHERE status IN ('available', 'missing') ORDER BY video_id";
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// The title `videos` knows for a video, for the download toasts.
    pub fn video_title(&self, video_id: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT title FROM videos WHERE video_id = ?1", [video_id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    /// Retry: an `error`, `missing` or `available` row back to `queued`, at the back of the
    /// queue. `false` for one already queued or running.
    pub fn requeue_download(&self, video_id: &str, format: &str, now: i64) -> bool {
        let stamp = rfc3339_utc(now);
        self.update_download(
            "UPDATE downloads SET status = 'queued', error = NULL, completed_at = NULL, \
             created_at = ?3 WHERE video_id = ?1 AND format = ?2 \
             AND status IN ('error', 'missing', 'available')",
            rusqlite::params![video_id, format, stamp],
        )
    }

    /// Forget a download. The row only: the file, if any, is never touched.
    pub fn delete_download(&self, video_id: &str, format: &str) -> bool {
        self.update_download(
            "DELETE FROM downloads WHERE video_id = ?1 AND format = ?2",
            rusqlite::params![video_id, format],
        )
    }

    /// The oldest queued download (FIFO by `created_at`).
    pub fn next_queued_download(&self) -> Option<DownloadRow> {
        let conn = self.0.lock().unwrap();
        let sql = format!("{DOWNLOAD_SELECT} WHERE status = 'queued' {DOWNLOAD_FIFO} LIMIT 1");
        conn.query_row(&sql, [], download_row).ok()
    }

    /// Every download row of these videos, both formats.
    pub fn downloads_for(&self, video_ids: &[String]) -> Vec<DownloadRow> {
        if video_ids.is_empty() {
            return Vec::new();
        }
        let conn = self.0.lock().unwrap();
        let holes = vec!["?"; video_ids.len()].join(",");
        let sql = format!("{DOWNLOAD_SELECT} WHERE video_id IN ({holes}) ORDER BY created_at");
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(video_ids), download_row) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// The newest `limit` downloads, by completion (or, until then, queueing) date.
    pub fn recent_downloads(&self, limit: usize) -> Vec<DownloadRow> {
        let conn = self.0.lock().unwrap();
        let sql = format!(
            "{DOWNLOAD_SELECT} ORDER BY COALESCE(completed_at, created_at) DESC, rowid DESC \
             LIMIT ?1"
        );
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([limit as i64], download_row) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Downloads left `running` by a run that never finished (the app quit or crashed) go back
    /// to `queued`. Answers how many. For the runner's start.
    pub fn reset_orphaned_downloads(&self) -> usize {
        let conn = self.0.lock().unwrap();
        let sql = "UPDATE downloads SET status = 'queued' WHERE status = 'running'";
        conn.execute(sql, []).unwrap_or(0)
    }

    fn update_download(&self, sql: &str, params: impl rusqlite::Params) -> bool {
        let conn = self.0.lock().unwrap();
        conn.execute(sql, params).map(|n| n > 0).unwrap_or(false)
    }
}

const DOWNLOAD_FIFO: &str = "ORDER BY created_at, rowid";

const DOWNLOAD_INSERT: &str = "INSERT INTO downloads(video_id, format, status, \
     requested_quality, thumbnail_mode, dest_dir, created_at) \
     VALUES(?1, ?2, 'queued', ?3, ?4, ?5, ?6)";

const DOWNLOAD_REQUEUE: &str = "UPDATE downloads SET status = 'queued', requested_quality = ?3, \
     thumbnail_mode = ?4, dest_dir = ?5, error = NULL, completed_at = NULL, created_at = ?6 \
     WHERE video_id = ?1 AND format = ?2";

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn top_plays_ranks_by_count_then_recency_and_carries_the_latest_metadata() {
        let d = db();
        // "a" twice, "b" three times, "c" once but most recently, "old" outside the window.
        for (id, json, at) in [
            ("old", "{\"old\":1}", 100),
            ("a", "{\"a\":1}", 1_000),
            ("a", "{\"a\":2}", 1_100),
            ("b", "{\"b\":1}", 1_000),
            ("b", "{\"b\":2}", 1_050),
            ("b", "{\"b\":3}", 1_060),
            ("c", "{\"c\":1}", 2_000),
        ] {
            // A window wide enough that inserting doesn't prune what the next row needs; the
            // "old" row is excluded by `since` below instead.
            d.record_play(id, json, at, 10_000);
        }

        let top = d.top_plays(900, 20);
        assert_eq!(
            top,
            vec![
                ("{\"b\":3}".into(), 3), // most plays
                ("{\"a\":2}".into(), 2), // latest json wins for a song, not the first
                ("{\"c\":1}".into(), 1), // ties on count break toward the recent play
            ],
            "'old' is outside the window and must not appear"
        );
        assert_eq!(d.top_plays(900, 2).len(), 2, "limit applies");

        // Same rows through `play_counts`: every song, not a top N, and no metadata.
        let mut counts = d.play_counts(900);
        counts.sort();
        assert_eq!(counts, vec![("a".into(), 2), ("b".into(), 3), ("c".into(), 1)]);
        assert!(
            d.play_counts(1_500) == vec![("c".into(), 1)],
            "`since` cuts the same way it does for top_plays"
        );
    }

    #[test]
    fn playlist_index_replaces_patches_and_prunes() {
        let d = db();
        d.set_playlist_tracks("VL1", &["a".into(), "b".into()]);
        d.set_playlist_tracks("VL2", &["b".into()]);

        let m = d.playlist_memberships();
        assert_eq!(m["a"], vec!["VL1"]);
        let mut b = m["b"].clone();
        b.sort();
        assert_eq!(b, vec!["VL1", "VL2"], "one track can sit in several playlists");

        // A re-crawl is the whole list, so a track it no longer saw has to disappear with it.
        d.set_playlist_tracks("VL1", &["a".into()]);
        assert_eq!(d.playlist_memberships()["b"], vec!["VL2"]);

        // Single-track patches, the path an add or a remove made inside the app takes.
        d.add_playlist_track("VL2", "a");
        d.add_playlist_track("VL2", "a"); // idempotent: the index may already know
        let mut a = d.playlist_memberships()["a"].clone();
        a.sort();
        assert_eq!(a, vec!["VL1", "VL2"]);
        d.remove_playlist_track("VL2", "a");
        assert_eq!(d.playlist_memberships()["a"], vec!["VL1"]);

        d.forget_playlist("VL2");
        assert!(!d.playlist_memberships().contains_key("b"), "VL2 held b alone");

        // Retain keeps the named playlists and drops everything else, including on an empty list.
        d.set_playlist_tracks("VL3", &["c".into()]);
        d.retain_playlists(&["VL3".into()]);
        assert_eq!(d.playlist_memberships().keys().collect::<Vec<_>>(), vec!["c"]);
        d.retain_playlists(&[]);
        assert!(d.playlist_memberships().is_empty());
    }

    #[test]
    fn local_playlists_hold_tracks_in_order_and_outlive_the_account_index() {
        let d = db();
        let song = |v: &str| (v.to_string(), format!(r#"{{"video_id":"{v}"}}"#));
        let a = d.create_local_playlist("Road trip", 10).unwrap();
        let b = d.create_local_playlist("Empty", 11).unwrap();
        assert_ne!(a, b);

        // A duplicate is refused per row, the rest of the batch still lands.
        let added = d.add_local_playlist_tracks(a, &[song("x"), song("y"), song("x")], 20).unwrap();
        assert_eq!(added, [true, true, false]);
        assert!(d.add_local_playlist_tracks(999, &[song("z")], 20).is_err(), "no such playlist");

        // Most recently changed first; the count and the first track come with the row.
        let all = d.local_playlists();
        assert_eq!(all.iter().map(|p| p.id).collect::<Vec<_>>(), [a, b]);
        assert_eq!((all[0].count, all[1].count), (2, 0));
        assert_eq!(all[0].first_song.as_deref(), Some(r#"{"video_id":"x"}"#));
        assert_eq!(all[1].first_song, None);
        let rows = d.local_playlist_tracks(a);
        assert_eq!(rows.iter().map(|r| r.1.contains('x')).collect::<Vec<_>>(), [true, false]);

        // The membership index names it by browseId, and the account-side prunes leave it alone.
        let key = format!("{}{a}", crate::state::LOCAL_PLAYLIST_PREFIX);
        d.set_playlist_tracks("VL1", &["x".into()]);
        d.clear_playlist_index();
        d.retain_playlists(&[]);
        assert_eq!(d.playlist_memberships()["x"], vec![key.clone()]);

        // A row id only removes inside its own playlist.
        d.remove_local_playlist_tracks(b, &[rows[0].0], 30).unwrap();
        assert_eq!(d.local_playlist_tracks(a).len(), 2);
        d.remove_local_playlist_tracks(a, &[rows[0].0], 30).unwrap();
        assert_eq!(d.local_playlist(a).unwrap().first_song.as_deref(), Some(r#"{"video_id":"y"}"#));
        assert!(!d.playlist_memberships().contains_key("x"));

        d.edit_local_playlist(a, Some("Renamed"), None, 40).unwrap();
        d.edit_local_playlist(a, None, Some("notes"), 41).unwrap();
        let p = d.local_playlist(a).unwrap();
        assert_eq!((p.title.as_str(), p.description.as_str()), ("Renamed", "notes"));
        assert!(d.edit_local_playlist(999, Some("x"), None, 42).is_err());

        d.delete_local_playlist(a).unwrap();
        assert!(d.local_playlist(a).is_none());
        assert!(d.local_playlist_tracks(a).is_empty(), "its rows go with it");
        // AUTOINCREMENT: a deleted playlist's number is never handed to the next one, not even
        // once the table is empty (a plain rowid would start again from 1).
        d.delete_local_playlist(b).unwrap();
        assert!(d.create_local_playlist("New", 50).unwrap() > b);
    }

    #[test]
    fn local_playlists_reorder_by_position_and_append_after_it() {
        let d = db();
        let song = |v: &str| (v.to_string(), format!(r#"{{"video_id":"{v}"}}"#));
        let a = d.create_local_playlist("Mix", 10).unwrap();
        let other = d.create_local_playlist("Other", 10).unwrap();
        d.add_local_playlist_tracks(a, &[song("x"), song("y"), song("z")], 20).unwrap();
        d.add_local_playlist_tracks(other, &[song("q")], 20).unwrap();
        let order = |d: &Db| -> Vec<String> {
            d.local_playlist_tracks(a)
                .into_iter()
                .map(|(_, j)| j.split('"').nth(3).unwrap().to_owned())
                .collect()
        };
        let ids: Vec<i64> = d.local_playlist_tracks(a).into_iter().map(|r| r.0).collect();
        let foreign = d.local_playlist_tracks(other)[0].0;

        // z, x named; y left out follows them. A row from another playlist is ignored.
        d.reorder_local_playlist(a, &[ids[2], foreign, ids[0]], 30).unwrap();
        assert_eq!(order(&d), ["z", "x", "y"]);
        assert_eq!(d.local_playlist_tracks(other).len(), 1, "the other playlist is untouched");
        assert_eq!(d.local_playlist(a).unwrap().first_song.as_deref(), Some(r#"{"video_id":"z"}"#));

        // A new track goes after the last position, not by row id.
        d.add_local_playlist_tracks(a, &[song("w")], 40).unwrap();
        assert_eq!(order(&d), ["z", "x", "y", "w"]);
    }

    #[test]
    fn indexed_songs_carry_first_seen() {
        let d = db();
        d.set_playlist_songs("VL1", &[("a".into(), "{}".into())]); // first read: undated
        d.put_playlist_song("VL2", "a", "{}"); // added through the app: now
        let local = d.create_local_playlist("Here", 1).unwrap();
        d.add_local_playlist_tracks(local, &[("a".into(), "{}".into())], 42).unwrap();
        let here = format!("{}{local}", crate::state::LOCAL_PLAYLIST_PREFIX);
        let seen: std::collections::HashMap<String, Option<i64>> =
            d.indexed_songs().into_iter().map(|(_, p, _, s)| (p, s)).collect();
        assert_eq!(seen["VL1"], None);
        assert!(seen["VL2"].is_some_and(|s| s > 1_000_000_000), "epoch seconds");
        assert_eq!(seen[&here], Some(42), "a local track's date is when it was added");
    }

    #[test]
    fn indexed_songs_and_alerts() {
        let d = db();
        d.set_playlist_songs("VL1", &[("a".into(), "{\"t\":1}".into()), ("b".into(), "{}".into())]);
        d.set_playlist_tracks("VL2", &["a".into()]); // no metadata: left out of the view
        d.put_playlist_song("VL2", "c", "{\"t\":3}");
        let local = d.create_local_playlist("Here", 1).unwrap();
        d.add_local_playlist_tracks(local, &[("a".into(), "{}".into())], 1).unwrap();
        let mut got: Vec<(String, String)> =
            d.indexed_songs().into_iter().map(|(v, p, _, _)| (v, p)).collect();
        got.sort();
        let here = format!("{}{local}", crate::state::LOCAL_PLAYLIST_PREFIX);
        let want: Vec<(String, String)> =
            [("a", here.as_str()), ("a", "VL1"), ("b", "VL1"), ("c", "VL2")]
                .iter()
                .map(|(v, p)| (v.to_string(), p.to_string()))
                .collect();
        assert_eq!(got, want);
        assert_eq!(d.playlist_songs("VL1").len(), 2);

        d.add_playlist_alert("VL1", "a", "removed", Some("{}"), 5);
        d.add_playlist_alert("VL1", "a", "removed", None, 6); // the same alert: ignored
        d.add_playlist_alert("VL1", "a", "gone", None, 6); // its pre-v3 name: the same alert
        d.add_playlist_alert(&here, "a", "unavailable", None, 7);
        assert_eq!(d.playlist_alerts().len(), 2);
        assert!(d.playlist_alerts().iter().all(|a| a.2 != "gone"));
        d.dismiss_playlist_alert("VL1", "a", "removed");
        d.add_playlist_alert("VL1", "a", "removed", None, 8); // dismissed stays dismissed
        assert_eq!(d.playlist_alerts().len(), 1);
        // An account change clears the account's alerts, not the local playlist's.
        d.clear_playlist_index();
        assert_eq!(d.playlist_alerts().iter().map(|a| a.0.clone()).collect::<Vec<_>>(), [here]);
    }

    #[test]
    fn playlist_ops_journal_keeps_the_newest_and_undoes_once() {
        let d = db();
        let first = d.record_playlist_op("remove", Some("acc"), "{}", "[]", 1).unwrap();
        for i in 0..PLAYLIST_OPS_KEPT {
            d.record_playlist_op("move", None, "{}", "[]", 2 + i).unwrap();
        }
        let all = d.playlist_ops();
        assert_eq!(all.len() as i64, PLAYLIST_OPS_KEPT);
        assert!(all.windows(2).all(|w| w[0].id > w[1].id), "newest first");
        assert!(d.playlist_op(first).is_none(), "the oldest was pruned");

        let last = all[0].id;
        assert!(d.mark_playlist_op_undone(last, 99).unwrap());
        assert!(!d.mark_playlist_op_undone(last, 100).unwrap(), "a second undo is refused");
        assert_eq!(d.playlist_op(last).unwrap().undone_at, Some(99));
    }

    #[test]
    fn opening_the_db_clears_local_files_out_of_on_repeat() {
        // 0.3.1 counted local plays before On Repeat excluded them; opening the db drops the rows.
        let path = std::env::temp_dir().join("limusic-plays-purge-test.sqlite");
        std::fs::remove_file(&path).ok();
        {
            let d = Db::open(&path).unwrap();
            // Piggybacking on the one file-backed test: `journal_mode` answers with a row, so
            // setting it via `pragma_update` would silently do nothing (and `:memory:` cannot be
            // WAL at all, which is why this can't live in its own in-memory test).
            let mode: String =
                d.0.lock().unwrap().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
            assert_eq!(mode, "wal");
            d.record_play("LOCAL:/music/a.mp3", "{\"local\":1}", 1_000, 10_000);
            d.record_play("dQw4w9WgXcQ", "{\"yt\":1}", 1_000, 10_000);
            assert_eq!(d.top_plays(0, 20).len(), 2, "both were recorded");
        }
        let d = Db::open(&path).unwrap();
        assert_eq!(
            d.top_plays(0, 20),
            vec![("{\"yt\":1}".to_string(), 1)],
            "only the YouTube play survives"
        );
        drop(d);
        std::fs::remove_file(&path).ok();
    }

    /// v1.0.0: Boidu switched off carries over as the first entry of the provider order, the
    /// cache is purged once, and from then on clearing it spares hand-picked lyrics.
    #[test]
    fn opening_the_db_migrates_the_boidu_switch_and_pins_survive_clears() {
        let path = std::env::temp_dir().join("limusic-lyrics-providers-test.sqlite");
        std::fs::remove_file(&path).ok();
        {
            let d = Db::open(&path).unwrap();
            let conn = d.0.lock().unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
            conn.execute("INSERT INTO settings(key, value) VALUES('lyrics_boidu', 'false')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO lyrics_cache VALUES('old', '{\"source\":\"LRCLIB\"}', 1)",
                [],
            )
            .unwrap();
        }
        let d = Db::open(&path).unwrap();
        assert_eq!(d.get_setting("lyrics_providers").as_deref(), Some("-boidu"));
        assert_eq!(d.get_setting("lyrics_boidu"), None);
        assert_eq!(d.get_lyrics("old", 2, 10), None, "purged once");

        d.put_lyrics("auto", Some("{\"source\":\"LRCLIB\"}"), 2);
        d.put_lyrics("picked", Some("{\"source\":\"Kugou\",\"pinned\":true}"), 2);
        d.put_lyrics("miss", None, 2);
        d.clear_lyrics_cache();
        assert_eq!(d.get_lyrics("auto", 2, 10), None);
        assert_eq!(d.get_lyrics("miss", 2, 10), None);
        assert!(d.get_lyrics("picked", 2, 10).is_some(), "a hand-picked source is not a cache");
        drop(d);
        std::fs::remove_file(&path).ok();
    }

    /// A cache row with only the fields a test cares about; the rest are the boring defaults.
    fn row(url: &str, expires_at: i64) -> CachedStream {
        CachedStream {
            url: url.to_owned(),
            itag: 251,
            expires_at,
            loudness_db: None,
            is_video: None,
            ping_url: None,
            ping_client: None,
            client: None,
        }
    }

    #[test]
    fn put_stream_drops_entries_that_have_already_expired() {
        let d = db();
        d.put_stream("stale", &row("https://x/1", 1_000), 900);
        d.put_stream("live", &row("https://x/2", 9_000), 900);
        assert!(d.get_stream("stale", 900).is_some(), "not expired yet at t=900");

        // t=2000: "stale" expired at 1_000, so writing anything now sweeps it.
        d.put_stream("fresh", &row("https://x/3", 8_000), 2_000);
        assert!(d.get_stream("stale", 2_000).is_none());
        assert!(d.get_stream("live", 2_000).is_some(), "unexpired rows survive the sweep");
        assert!(d.get_stream("fresh", 2_000).is_some(), "the row just written survives it");
    }

    /// A cache hit skips `/player`, so the music-video verdict has to survive the round trip or
    /// the player view can't tell whether to load the video for a track played twice in a session.
    #[test]
    fn put_stream_round_trips_the_music_video_verdict() {
        let d = db();
        d.put_stream(
            "mv",
            &CachedStream { is_video: Some(true), ..row("https://x/1", 9_000) },
            900,
        );
        d.put_stream(
            "song",
            &CachedStream { is_video: Some(false), ..row("https://x/2", 9_000) },
            900,
        );
        d.put_stream("unknown", &row("https://x/3", 9_000), 900);
        assert_eq!(d.get_stream("mv", 900).unwrap().is_video, Some(true));
        assert_eq!(d.get_stream("song", 900).unwrap().is_video, Some(false));
        assert_eq!(d.get_stream("unknown", 900).unwrap().is_video, None);
    }

    /// The watch-history ping has to survive the cache the same way (issue #83): a hit skips
    /// `/player`, and the gapless lookahead means a track's *first* play is often a cache hit.
    #[test]
    fn put_stream_round_trips_the_watch_history_ping() {
        let d = db();
        d.put_stream(
            "pinged",
            &CachedStream {
                ping_url: Some("https://s.youtube.com/api/stats/playback?docid=x".to_owned()),
                ping_client: Some("ANDROID_VR_1_65_10".to_owned()),
                ..row("https://x/1", 9_000)
            },
            900,
        );
        d.put_stream("unpinged", &row("https://x/2", 9_000), 900);

        let hit = d.get_stream("pinged", 900).unwrap();
        assert_eq!(
            hit.ping_url.as_deref(),
            Some("https://s.youtube.com/api/stats/playback?docid=x")
        );
        assert_eq!(hit.ping_client.as_deref(), Some("ANDROID_VR_1_65_10"));

        let none = d.get_stream("unpinged", 900).unwrap();
        assert!(none.ping_url.is_none() && none.ping_client.is_none());
    }

    /// A replay rebuilds its headers from the recorded client, so it has to survive the cache.
    /// No test for a pre-column row reading as `None`: the migration wipes those.
    #[test]
    fn a_cached_stream_round_trips_its_client() {
        let d = db();
        d.put_stream(
            "v",
            &CachedStream { client: Some("VISIONOS".into()), ..row("https://x/1", 9_000) },
            900,
        );
        assert_eq!(d.get_stream("v", 900).unwrap().client.as_deref(), Some("VISIONOS"));
    }

    #[test]
    fn record_play_prunes_outside_the_window() {
        let d = db();
        d.record_play("stale", "{}", 1_000, 60);
        d.record_play("fresh", "{}", 5_000, 60); // prunes anything before 4_940
        assert_eq!(d.top_plays(0, 20), vec![("{}".to_string(), 1)]);
    }

    /// The key follows the Google account, not the jar: the `__Secure-3PAPISID` alias resolves to
    /// the same one, and a jar with no SAPISID is not an account at all.
    #[test]
    fn account_key_needs_a_sapisid() {
        assert_eq!(account_key("SID=abc; PREF=xyz"), None);
        let sapisid = account_key("SAPISID=secret123; SID=abc").unwrap();
        let secure = account_key("__Secure-3PAPISID=secret123; SID=abc").unwrap();
        assert_eq!(sapisid, secure, "the alias resolves to the same key");
        assert_ne!(sapisid, account_key("SAPISID=other").unwrap());
    }

    /// A re-login refreshes an account's row without resetting its place in the list, and a
    /// cookie from a different Google account lands in its own row keyed on its SAPISID.
    #[test]
    fn accounts_round_trip_refresh_and_remove() {
        let d = db();
        let a = StoredAccount {
            id: account_key("SAPISID=aaa").unwrap(),
            session_cookie: "SAPISID=aaa".into(),
            data_sync_id: Some("channel-a".into()),
            selected_identity_json: Some(r#"{"data_sync_id":"channel-a"}"#.into()),
            account_json: Some(r#"{"name":"A"}"#.into()),
            visitor_data: Some("vd-a".into()),
            added_at: 100,
        };
        d.upsert_account(&a).unwrap();
        assert_ne!(a.id, account_key("SAPISID=bbb").unwrap(), "keys follow the Google account");

        d.upsert_account(&StoredAccount {
            id: account_key("SAPISID=bbb").unwrap(),
            session_cookie: "SAPISID=bbb".into(),
            data_sync_id: Some("channel-b".into()),
            selected_identity_json: Some(r#"{"data_sync_id":"channel-b"}"#.into()),
            account_json: Some(r#"{"name":"B"}"#.into()),
            visitor_data: None,
            added_at: 200,
        })
        .unwrap();
        assert_eq!(d.list_accounts().len(), 2);

        // Same Google account re-login: refreshes the row, keeps its original `added_at`.
        d.upsert_account(&StoredAccount {
            id: a.id.clone(),
            session_cookie: "SAPISID=aaa; __Secure-3PSID=new".into(),
            data_sync_id: Some("channel-a".into()),
            selected_identity_json: Some(r#"{"data_sync_id":"channel-a"}"#.into()),
            account_json: Some(r#"{"name":"A renamed"}"#.into()),
            visitor_data: Some("vd-a2".into()),
            added_at: 999,
        })
        .unwrap();
        let accounts = d.list_accounts();
        assert_eq!(accounts.len(), 2);
        let refreshed = accounts.iter().find(|acc| acc.id == a.id).unwrap();
        assert_eq!(refreshed.session_cookie, "SAPISID=aaa; __Secure-3PSID=new");
        assert_eq!(refreshed.account_json.as_deref(), Some(r#"{"name":"A renamed"}"#));
        assert_eq!(refreshed.added_at, 100, "re-login must not reorder the list");
        assert_eq!(d.get_account(&a.id).unwrap().visitor_data.as_deref(), Some("vd-a2"));

        // Rotation writes the jar back without disturbing the identity fields.
        d.update_account_cookie(&a.id, "SAPISID=aaa; __Secure-3PSIDTS=rotated");
        let rotated = d.get_account(&a.id).unwrap();
        assert_eq!(rotated.session_cookie, "SAPISID=aaa; __Secure-3PSIDTS=rotated");
        assert_eq!(rotated.account_json.as_deref(), Some(r#"{"name":"A renamed"}"#));

        d.remove_account(&a.id);
        assert!(d.get_account(&a.id).is_none());
        assert_eq!(d.list_accounts().len(), 1);
    }

    /// Switching to a saved account flips the `active_account` pointer in the same transaction as
    /// the restored projections, so a crash between them can't leave the pointer disagreeing with
    /// the projections a restart will actually use.
    #[test]
    fn restore_account_moves_the_active_pointer_with_the_projections() {
        let d = db();
        let account = StoredAccount {
            id: account_key("SAPISID=bbb").unwrap(),
            session_cookie: "SAPISID=bbb".into(),
            data_sync_id: Some("channel-b".into()),
            selected_identity_json: Some(r#"{"data_sync_id":"channel-b"}"#.into()),
            account_json: Some(r#"{"name":"B"}"#.into()),
            visitor_data: Some("vd-b".into()),
            added_at: 200,
        };
        d.upsert_account(&account).unwrap();
        d.set_setting("active_account", &account_key("SAPISID=aaa").unwrap());

        d.restore_account(&account).unwrap();
        assert_eq!(
            d.get_setting("active_account").as_deref(),
            Some(account.id.as_str()),
            "the pointer flips alongside the restored projections"
        );
        assert_eq!(d.get_setting("session_cookie").as_deref(), Some("SAPISID=bbb"));
        assert_eq!(d.get_setting("data_sync_id").as_deref(), Some("channel-b"));
        assert_eq!(
            d.get_setting("selected_identity_json").as_deref(),
            Some(r#"{"data_sync_id":"channel-b"}"#)
        );
        assert_eq!(d.get_setting("account_json").as_deref(), Some(r#"{"name":"B"}"#));
        assert_eq!(d.get_setting("visitor_data").as_deref(), Some("vd-b"));
        assert_eq!(d.get_setting("account_selection_pending"), None);
    }

    /// Rows filed under a key the current `account_key` no longer computes (the pre-release SHA-1
    /// scheme) are folded onto their canonical id on open, and a duplicate of an account that is
    /// already there is merged away rather than left to show up twice in the menu.
    #[test]
    fn opening_the_db_rekeys_and_dedupes_accounts() {
        let path = std::env::temp_dir().join("limusic-accounts-rekey-test.sqlite");
        std::fs::remove_file(&path).ok();
        let canonical = account_key("SAPISID=aaa").unwrap();
        {
            let d = Db::open(&path).unwrap();
            let conn = d.0.lock().unwrap();
            // Two accounts under stale ids; only one of them also has a canonical row.
            for (id, cookie, added_at) in [
                ("ga-staleaaa", "SAPISID=aaa", 100),
                (canonical.as_str(), "SAPISID=aaa", 500),
                ("ga-stalebbb", "SAPISID=bbb", 200),
            ] {
                conn.execute(
                    "INSERT INTO accounts(id, session_cookie, data_sync_id, \
                     selected_identity_json, account_json, visitor_data, added_at) \
                     VALUES(?1, ?2, NULL, NULL, NULL, NULL, ?3)",
                    rusqlite::params![id, cookie, added_at],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO settings(key, value) VALUES('active_account', 'ga-stalebbb')",
                [],
            )
            .unwrap();
        }
        {
            let d = Db::open(&path).unwrap();
            let accounts = d.list_accounts();
            assert_eq!(accounts.len(), 2, "the duplicate of SAPISID=aaa is merged away");
            let a = d.get_account(&canonical).unwrap();
            assert_eq!(a.added_at, 100, "the surviving row keeps the earlier place in the list");
            let b = account_key("SAPISID=bbb").unwrap();
            assert!(d.get_account(&b).is_some(), "a stale id with no canonical row is moved");
            assert_eq!(
                d.get_setting("active_account").as_deref(),
                Some(b.as_str()),
                "the active pointer follows the move"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    /// Databases written before multi-account migrate their single session into `accounts` once.
    #[test]
    fn opening_the_db_migrates_the_legacy_session() {
        let path = std::env::temp_dir().join("limusic-accounts-migration-test.sqlite");
        std::fs::remove_file(&path).ok();
        {
            let d = Db::open(&path).unwrap();
            d.set_setting("session_cookie", "SAPISID=legacy");
            d.set_setting("data_sync_id", "channel-x");
            d.set_setting("account_json", r#"{"name":"Legacy"}"#);
            d.set_setting("visitor_data", "vd-x");
        }
        {
            let d = Db::open(&path).unwrap();
            let accounts = d.list_accounts();
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0].session_cookie, "SAPISID=legacy");
            assert_eq!(accounts[0].data_sync_id.as_deref(), Some("channel-x"));
            assert_eq!(
                d.get_setting("active_account").as_deref(),
                Some(accounts[0].id.as_str()),
                "the migrated session is the active account"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    /// A fresh directory per test: these run in parallel and each one moves files around.
    fn qtest_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("limusic-qtest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_or_quarantine_moves_a_corrupt_file_aside() {
        let dir = qtest_dir("corrupt");
        let path = dir.join("limusic.sqlite");
        std::fs::write(&path, b"this is not a sqlite file").unwrap();
        {
            let (d, aside) = Db::open_or_quarantine(&path).unwrap();
            let aside = aside.expect("a corrupt file is moved aside");
            assert_eq!(std::fs::read(&aside).unwrap(), b"this is not a sqlite file");
            d.set_setting("k", "v");
        }
        let d = Db::open(&path).unwrap();
        assert_eq!(d.get_setting("k").as_deref(), Some("v"), "the fresh file is a working db");
        drop(d);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The recovery path must never fire on a healthy database: it would hand the user an empty
    /// library for nothing.
    #[test]
    fn open_or_quarantine_leaves_a_healthy_file_alone() {
        let dir = qtest_dir("healthy");
        let path = dir.join("limusic.sqlite");
        Db::open(&path).unwrap().set_setting("k", "v");
        let (d, aside) = Db::open_or_quarantine(&path).unwrap();
        assert_eq!(aside, None);
        assert_eq!(d.get_setting("k").as_deref(), Some("v"));
        let moved = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(!moved, "nothing was moved aside");
        drop(d);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Moving a healthy library aside because it could not be *opened* (locked, permissions) would
    /// cost the user everything for nothing. A directory at the path is a portable stand-in for
    /// "cannot open, but not corrupt".
    #[test]
    fn open_or_quarantine_leaves_an_unopenable_path_alone() {
        let dir = qtest_dir("unopenable");
        let path = dir.join("limusic.sqlite");
        std::fs::create_dir(&path).unwrap();
        assert!(Db::open_or_quarantine(&path).is_err());
        assert!(path.is_dir(), "nothing was moved aside");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// SQLite deletes stray -wal/-shm files itself while failing to open a corrupt main file, so
    /// they are usually gone before the rename loop runs; the loop is a backstop for any it
    /// leaves. Either way, the outcome that matters is that the fresh file does not inherit them.
    #[test]
    fn open_or_quarantine_drops_the_stale_wal_sidecars() {
        let dir = qtest_dir("sidecars");
        let path = dir.join("limusic.sqlite");
        std::fs::write(&path, b"this is not a sqlite file").unwrap();
        std::fs::write(dir.join("limusic.sqlite-wal"), b"stale wal").unwrap();
        std::fs::write(dir.join("limusic.sqlite-shm"), b"stale shm").unwrap();
        let (d, aside) = Db::open_or_quarantine(&path).unwrap();
        assert!(aside.is_some_and(|a| a.exists()), "the corrupt file is moved aside");
        for suffix in ["-wal", "-shm"] {
            let now =
                std::fs::read(dir.join(format!("limusic.sqlite{suffix}"))).unwrap_or_default();
            assert!(!now.starts_with(b"stale"), "the fresh db inherited the old {suffix}");
        }
        drop(d);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- upgrading a database an older release wrote ------------------------------------------
    //
    // Every other file-backed test here starts from `Db::open`, which means from the *current*
    // schema, so none of them ever runs the ALTERs and one-shot migrations in `open` against a
    // file that actually needs them. These do. Each constant is the `execute_batch` SQL one shipped
    // release ran, copied verbatim from `git show <tag>:src-tauri/src/db.rs`. They are pinned as
    // literals on purpose: a schema generated from the current code would drift along with it and
    // stop being a record of what users' files really look like, which is the only thing that
    // makes the test worth having. Never edit one to make a test pass; add a new tag instead.
    //
    // Why these three:
    // - v0.4.11: `stream_url_cache` is five columns, `local_tracks` has no `album_artist`, no
    //   `accounts`. Every ALTER in `open` has work to do and both cache wipes fire. The long tail:
    //   .deb, .rpm and AUR installs never self-update, so some users really are on it.
    // - v0.5.12: has `is_video` and the ping columns but still no `accounts`, so it isolates the
    //   legacy single-session migration from most of the column work.
    // - v0.8.1: the predecessor of 0.8.2, which is where most self-updating installs sit. It lacks
    //   only `client`, so the one thing opening it does is the `client` wipe. The common path.

    const SCHEMA_V0_4_11: &str = r#"
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS stream_url_cache (
                video_id    TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                itag        INTEGER NOT NULL,
                expires_at  INTEGER NOT NULL,
                loudness_db REAL
            );
            CREATE TABLE IF NOT EXISTS lyrics_cache (
                video_id   TEXT PRIMARY KEY,
                lyrics     TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plays (
                id        INTEGER PRIMARY KEY,
                video_id  TEXT NOT NULL,
                played_at INTEGER NOT NULL,
                song_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS plays_played_at ON plays(played_at);
            CREATE TABLE IF NOT EXISTS local_tracks (
                path          TEXT PRIMARY KEY,
                title         TEXT NOT NULL,
                artist        TEXT NOT NULL,
                album         TEXT NOT NULL,
                album_key     TEXT NOT NULL,
                track_no      INTEGER NOT NULL,
                duration_secs INTEGER NOT NULL,
                cover         TEXT,
                mtime         INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS local_tracks_album ON local_tracks(album_key);
            CREATE TABLE IF NOT EXISTS playlist_track (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                PRIMARY KEY (playlist_id, video_id)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS playlist_track_video ON playlist_track(video_id);
            "#;

    const SCHEMA_V0_5_12: &str = r#"
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS stream_url_cache (
                video_id    TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                itag        INTEGER NOT NULL,
                expires_at  INTEGER NOT NULL,
                loudness_db REAL,
                is_video    INTEGER,
                ping_url    TEXT,
                ping_client TEXT
            );
            CREATE TABLE IF NOT EXISTS lyrics_cache (
                video_id   TEXT PRIMARY KEY,
                lyrics     TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plays (
                id        INTEGER PRIMARY KEY,
                video_id  TEXT NOT NULL,
                played_at INTEGER NOT NULL,
                song_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS plays_played_at ON plays(played_at);
            CREATE TABLE IF NOT EXISTS local_tracks (
                path          TEXT PRIMARY KEY,
                title         TEXT NOT NULL,
                artist        TEXT NOT NULL,
                album         TEXT NOT NULL,
                album_key     TEXT NOT NULL,
                album_artist  TEXT,
                track_no      INTEGER NOT NULL,
                duration_secs INTEGER NOT NULL,
                cover         TEXT,
                mtime         INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS local_tracks_album ON local_tracks(album_key);
            CREATE TABLE IF NOT EXISTS playlist_track (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                PRIMARY KEY (playlist_id, video_id)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS playlist_track_video ON playlist_track(video_id);
            "#;

    const SCHEMA_V0_8_1: &str = r#"
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS stream_url_cache (
                video_id    TEXT PRIMARY KEY,
                url         TEXT NOT NULL,
                itag        INTEGER NOT NULL,
                expires_at  INTEGER NOT NULL,
                loudness_db REAL,
                is_video    INTEGER,
                ping_url    TEXT,
                ping_client TEXT
            );
            CREATE TABLE IF NOT EXISTS lyrics_cache (
                video_id   TEXT PRIMARY KEY,
                lyrics     TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plays (
                id        INTEGER PRIMARY KEY,
                video_id  TEXT NOT NULL,
                played_at INTEGER NOT NULL,
                song_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS plays_played_at ON plays(played_at);
            CREATE TABLE IF NOT EXISTS local_tracks (
                path          TEXT PRIMARY KEY,
                title         TEXT NOT NULL,
                artist        TEXT NOT NULL,
                album         TEXT NOT NULL,
                album_key     TEXT NOT NULL,
                album_artist  TEXT,
                track_no      INTEGER NOT NULL,
                duration_secs INTEGER NOT NULL,
                cover         TEXT,
                mtime         INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS local_tracks_album ON local_tracks(album_key);
            CREATE TABLE IF NOT EXISTS playlist_track (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                PRIMARY KEY (playlist_id, video_id)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS playlist_track_video ON playlist_track(video_id);
            CREATE TABLE IF NOT EXISTS accounts (
                id                     TEXT PRIMARY KEY,
                session_cookie         TEXT NOT NULL,
                data_sync_id           TEXT,
                selected_identity_json TEXT,
                account_json           TEXT,
                visitor_data           TEXT,
                added_at               INTEGER NOT NULL
            );
            "#;

    const LEGACY_COOKIE: &str = "SAPISID=legacy-sapisid; SID=legacy-sid";

    /// Builds a database file the way the release behind `schema` left it, then runs everything a
    /// user upgrading from it would need to be true, through `Db` itself.
    ///
    /// The old file is written with raw rusqlite, never `Db`, which would migrate it before the
    /// test got a look. The seeded rows use only columns the oldest schema has, so the same seed
    /// fits all three. `migrated_account` says whether that release had already moved the sign-in
    /// into `accounts` (0.8.x did, on its own first launch); either way the legacy `settings` rows
    /// are there, because every release keeps them as projections of the active account.
    fn assert_upgrades_cleanly(tag: &str, schema: &str, migrated_account: bool) {
        let dir = qtest_dir(&format!("upgrade-{tag}"));
        let path = dir.join("limusic.sqlite");
        let now = now_secs();
        let later = now + 6 * 3600;
        let id = account_key(LEGACY_COOKIE).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(schema).unwrap();
            for (key, value) in [
                ("volume", "0.42"),
                ("session_cookie", LEGACY_COOKIE),
                ("data_sync_id", "channel-legacy"),
                ("selected_identity_json", r#"{"data_sync_id":"channel-legacy"}"#),
                ("account_json", r#"{"name":"Legacy"}"#),
                ("visitor_data", "vd-legacy"),
            ] {
                conn.execute("INSERT INTO settings(key, value) VALUES(?1, ?2)", [key, value])
                    .unwrap();
            }
            if migrated_account {
                conn.execute(
                    "INSERT INTO accounts(id, session_cookie, data_sync_id, \
                     selected_identity_json, account_json, visitor_data, added_at) \
                     VALUES(?1, ?2, 'channel-legacy', '{\"data_sync_id\":\"channel-legacy\"}', \
                     '{\"name\":\"Legacy\"}', 'vd-legacy', 100)",
                    [&id, LEGACY_COOKIE],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO settings(key, value) VALUES('active_account', ?1)",
                    [&id],
                )
                .unwrap();
            }
            // Unexpired, so if it disappears it was the migration's wipe and not the expiry sweep.
            conn.execute(
                "INSERT INTO stream_url_cache(video_id, url, itag, expires_at, loudness_db) \
                 VALUES('old-row', 'https://x/old', 251, ?1, -3.5)",
                [later],
            )
            .unwrap();
            for (video_id, song_json) in
                [("dQw4w9WgXcQ", r#"{"yt":1}"#), ("LOCAL:/music/a.mp3", r#"{"local":1}"#)]
            {
                conn.execute(
                    "INSERT INTO plays(video_id, played_at, song_json) VALUES(?1, 1000, ?2)",
                    [video_id, song_json],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO local_tracks(path, title, artist, album, album_key, track_no, \
                 duration_secs, cover, mtime) \
                 VALUES('/music/a.mp3', 'A', 'Artist', 'Album', 'artist--album', 1, 200, NULL, 5)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO playlist_track(playlist_id, video_id) VALUES('VL1', 'dQw4w9WgXcQ')",
                [],
            )
            .unwrap();
        }

        // Checkpoints 1 and 6 in one call: `open_or_quarantine` tries `Db::open` first and returns
        // `None` only when that succeeded, so this is both "it opens" and "an old file is not
        // mistaken for a damaged one and moved aside, taking the library with it".
        let (d, aside) = Db::open_or_quarantine(&path).unwrap();
        assert_eq!(aside, None, "{tag}: an old but healthy database was quarantined");

        // Checkpoint 2, through the real accessors rather than `PRAGMA table_info`: `put_stream`
        // and `get_stream` name every column the current code reads and writes, and both swallow
        // errors, so a column the migration failed to add shows up here as a row that never
        // comes back.
        let fresh = CachedStream {
            url: "https://x/fresh".into(),
            itag: 251,
            expires_at: later,
            loudness_db: Some(-2.0),
            is_video: Some(true),
            ping_url: Some("https://s.youtube.com/api/stats/playback".into()),
            ping_client: Some("WEB_REMIX".into()),
            client: Some("VISIONOS".into()),
        };
        d.put_stream("fresh", &fresh, now);
        let got = d.get_stream("fresh", now).unwrap_or_else(|| {
            panic!("{tag}: the current cache query does not work on the upgraded file")
        });
        assert_eq!(
            (got.url.as_str(), got.loudness_db, got.is_video, got.client.as_deref()),
            ("https://x/fresh", Some(-2.0), Some(true), Some("VISIONOS")),
            "{tag}"
        );
        assert_eq!(got.ping_client.as_deref(), Some("WEB_REMIX"), "{tag}");
        // Every one of these releases predates `client`, so the pre-column row has to be gone:
        // its headers cannot be rebuilt. Checked after the round trip above, which is what
        // proves `None` here means "deleted" and not "the query failed".
        assert!(d.get_stream("old-row", now).is_none(), "{tag}: the stale cache row survived");

        // Checkpoint 3: the user's history and library come through untouched, except the local
        // play, which `open` removes on purpose (On Repeat excludes local files since 0.3.1).
        assert_eq!(d.get_setting("volume").as_deref(), Some("0.42"), "{tag}");
        assert_eq!(d.top_plays(0, 20), vec![(r#"{"yt":1}"#.to_string(), 1)], "{tag}");
        let tracks = d.local_tracks(None);
        assert_eq!(tracks.len(), 1, "{tag}");
        assert_eq!((tracks[0].path.as_str(), tracks[0].title.as_str()), ("/music/a.mp3", "A"));
        assert_eq!(tracks[0].album_artist, None, "{tag}: a new column reads as unknown");
        assert_eq!(d.playlist_memberships()["dQw4w9WgXcQ"], vec!["VL1"], "{tag}");
        // The playlists-on-this-device tables are new in 1.0.0, so every older file must get them.
        let mine = d.create_local_playlist("Mine", now).unwrap();
        assert_eq!(
            d.add_local_playlist_tracks(mine, &[("v".into(), "{}".into())], now).unwrap(),
            [true]
        );

        // Checkpoint 4: still signed in. One account, keyed the way this build computes it,
        // carrying the identity the old release stored, and pointed at as the active one.
        let accounts = d.list_accounts();
        assert_eq!(accounts.len(), 1, "{tag}: expected exactly the one signed-in account");
        let a = &accounts[0];
        assert_eq!(a.id, id, "{tag}");
        assert_eq!(a.session_cookie, LEGACY_COOKIE, "{tag}");
        assert_eq!(a.data_sync_id.as_deref(), Some("channel-legacy"), "{tag}");
        assert_eq!(
            a.selected_identity_json.as_deref(),
            Some(r#"{"data_sync_id":"channel-legacy"}"#),
            "{tag}"
        );
        assert_eq!(a.account_json.as_deref(), Some(r#"{"name":"Legacy"}"#), "{tag}");
        assert_eq!(a.visitor_data.as_deref(), Some("vd-legacy"), "{tag}");
        if migrated_account {
            assert_eq!(a.added_at, 100, "{tag}: an existing account row was rewritten");
        }
        assert_eq!(d.get_setting("active_account").as_deref(), Some(id.as_str()), "{tag}");
        assert_eq!(d.get_setting("session_cookie").as_deref(), Some(LEGACY_COOKIE), "{tag}");
        drop(d);

        // Checkpoint 5: the wipes are one-shot. They key off an ALTER succeeding, so if one ever
        // reported success on a file that already had the column, every launch would empty the
        // cache and nothing else would notice. The row written above has to outlive a relaunch.
        let d = Db::open(&path).unwrap();
        assert!(d.get_stream("fresh", now).is_some(), "{tag}: the cache was wiped again");
        assert_eq!(d.list_accounts().len(), 1, "{tag}: a relaunch duplicated the account");
        drop(d);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_v0_4_11_database_upgrades_cleanly() {
        assert_upgrades_cleanly("v0.4.11", SCHEMA_V0_4_11, false);
    }

    #[test]
    fn a_v0_5_12_database_upgrades_cleanly() {
        assert_upgrades_cleanly("v0.5.12", SCHEMA_V0_5_12, false);
    }

    #[test]
    fn a_v0_8_1_database_upgrades_cleanly() {
        assert_upgrades_cleanly("v0.8.1", SCHEMA_V0_8_1, true);
    }

    #[test]
    fn auth_identity_projections_are_updated_and_cleared_together() {
        let d = db();
        d.set_auth_identity(
            "SAPISID=cookie-a",
            r#"{"data_sync_id":"channel-a"}"#,
            Some("channel-a"),
            r#"{"name":"Channel A"}"#,
        )
        .unwrap();
        assert_eq!(d.get_setting("data_sync_id").as_deref(), Some("channel-a"));
        assert_eq!(
            d.get_setting("selected_identity_json").as_deref(),
            Some(r#"{"data_sync_id":"channel-a"}"#)
        );
        assert_eq!(d.get_setting("account_json").as_deref(), Some(r#"{"name":"Channel A"}"#));
        assert_eq!(d.get_setting("session_cookie").as_deref(), Some("SAPISID=cookie-a"));

        d.set_pending_auth_selection("SAPISID=cookie-b", None).unwrap();
        assert_eq!(d.get_setting("session_cookie").as_deref(), Some("SAPISID=cookie-b"));
        assert_eq!(d.get_setting("selected_identity_json"), None);
        assert_eq!(d.get_setting("data_sync_id"), None);
        assert_eq!(d.get_setting("account_json"), None);
        assert_eq!(d.get_setting("account_selection_pending").as_deref(), Some("true"));

        d.set_pending_auth_selection("SAPISID=cookie-c", Some("ga-c")).unwrap();
        assert_eq!(d.get_setting("active_account").as_deref(), Some("ga-c"));

        d.set_auth_identity(
            "SAPISID=cookie-b",
            r#"{"data_sync_id":null}"#,
            None,
            r#"{"name":"Single channel"}"#,
        )
        .unwrap();
        assert_eq!(d.get_setting("data_sync_id"), None, "a stale delegated id must be deleted");
        assert_eq!(d.get_setting("account_selection_pending"), None);

        d.clear_auth_identity().unwrap();
        assert_eq!(d.get_setting("selected_identity_json"), None);
        assert_eq!(d.get_setting("data_sync_id"), None);
        assert_eq!(d.get_setting("account_json"), None);
    }

    // --- schema v3 ------------------------------------------------------------------------------

    /// The monitor's tables as a v2 file has them, pinned like the release schemas above, with a
    /// few rows. `Db::open` creates everything else.
    const SCHEMA_V2_MONITOR: &str = r#"
            CREATE TABLE settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE playlist_track (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                song_json   TEXT,
                PRIMARY KEY (playlist_id, video_id)
            ) WITHOUT ROWID;
            CREATE TABLE playlist_alert (
                playlist_id TEXT NOT NULL,
                video_id    TEXT NOT NULL,
                kind        TEXT NOT NULL,
                song_json   TEXT,
                at          INTEGER NOT NULL,
                dismissed   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (playlist_id, video_id, kind)
            ) WITHOUT ROWID;
            INSERT INTO playlist_track VALUES('VL1', 'a', '{}');
            INSERT INTO playlist_alert VALUES('VL1', 'a', 'gone', '{}', 5, 0);
            INSERT INTO playlist_alert VALUES('VL1', 'b', 'gone', NULL, 6, 1);
            INSERT INTO playlist_alert VALUES('VL2', 'c', 'unavailable', NULL, 7, 0);
            PRAGMA user_version = 2;
            "#;

    fn snap(v: &str) -> SnapItem {
        SnapItem {
            v: v.to_owned(),
            s: Some(format!("S{v}")),
            t: v.to_uppercase(),
            a: "Artist".into(),
            d: Some("3:00".into()),
            u: false,
            th: None,
        }
    }

    fn run(started_at: i64, trigger: &str, outcome: &str) -> MonitorRun {
        MonitorRun {
            id: 0,
            started_at,
            finished_at: started_at + 5,
            trigger: trigger.into(),
            outcome: outcome.into(),
            playlists_ok: 3,
            playlists_failed: 0,
            alerts_new: 1,
            units_spent: 0,
            detail_json: "{}".into(),
        }
    }

    fn user_version(d: &Db) -> i64 {
        d.0.lock().unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    fn first_seen(d: &Db, playlist_id: &str, video_id: &str) -> Option<i64> {
        let conn = d.0.lock().unwrap();
        conn.query_row(
            "SELECT first_seen FROM playlist_track WHERE playlist_id = ?1 AND video_id = ?2",
            [playlist_id, video_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_v2_database_migrates_to_v3_once_and_keeps_its_alerts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        rusqlite::Connection::open(&path).unwrap().execute_batch(SCHEMA_V2_MONITOR).unwrap();

        let d = Db::open(&path).unwrap();
        assert_eq!(user_version(&d), 4, "on through v3 to v4");
        let rows = d.alert_rows(true);
        let kinds: Vec<(&str, &str, bool, bool)> = rows
            .iter()
            .map(|r| (r.video_id.as_str(), r.kind.as_str(), r.seen, r.dismissed))
            .collect();
        assert_eq!(
            kinds,
            [
                ("c", "unavailable", false, false),
                ("b", "removed", true, true),
                ("a", "removed", false, false),
            ],
            "newest first, `gone` is `removed`, and dismissed counts as seen"
        );
        assert_eq!(d.unseen_alert_count(), 2);
        // The migrated key is the old identity, so an alert already filed does not come back.
        d.add_playlist_alert("VL1", "a", "removed", None, 9);
        assert_eq!(d.alert_rows(true).len(), 3);
        assert_eq!(first_seen(&d, "VL1", "a"), None, "in the playlist since before tracking");
        assert!(d.put_snapshot_if_changed("VL1", None, None, 10, &[snap("a")]).is_some());
        d.record_monitor_run(&run(10, "manual_ui", "ok")).unwrap();
        drop(d);

        // Opening again is a no-op, and so is running the whole migration again.
        let d = Db::open(&path).unwrap();
        assert_eq!(d.alert_rows(true).len(), 3);
        d.0.lock().unwrap().execute_batch("PRAGMA user_version = 2").unwrap();
        drop(d);
        let d = Db::open(&path).unwrap();
        assert_eq!(user_version(&d), 4, "on through v3 to v4");
        assert_eq!(d.alert_rows(true).len(), 3);
        assert_eq!(d.snapshots("VL1").len(), 1);
        assert_eq!(d.monitor_runs(10).len(), 1);
    }

    #[test]
    fn a_file_upstream_numbered_v3_without_our_tables_still_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        let schema = SCHEMA_V2_MONITOR.replace("user_version = 2", "user_version = 3");
        rusqlite::Connection::open(&path).unwrap().execute_batch(&schema).unwrap();

        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 4, "on through v3 to v4");
        {
            let conn = d.0.lock().unwrap();
            assert!(v3_complete(&conn), "every v3 table and column is there");
            for (table, column) in [
                ("playlist_alert", "dedupe_key"),
                ("playlist_alert", "seen"),
                ("playlist_track", "first_seen"),
            ] {
                assert!(has_column(&conn, table, column).unwrap(), "{table}.{column}");
            }
        }
        let rows = d.alert_rows(true);
        let kinds: Vec<(&str, &str)> =
            rows.iter().map(|r| (r.video_id.as_str(), r.kind.as_str())).collect();
        assert_eq!(
            kinds,
            [("c", "unavailable"), ("b", "removed"), ("a", "removed")],
            "rows kept, `gone` is `removed`"
        );
        let conn = d.0.lock().unwrap();
        let tracks: i64 =
            conn.query_row("SELECT COUNT(*) FROM playlist_track", [], |r| r.get(0)).unwrap();
        assert_eq!(tracks, 1);
        drop(conn);
        assert!(d.put_snapshot_if_changed("VL1", None, None, 10, &[snap("a")]).is_some());
        d.record_monitor_run(&run(10, "manual_ui", "ok")).unwrap();
    }

    #[test]
    fn a_complete_v3_file_numbered_past_3_keeps_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        let d = Db::open(&path).unwrap();
        d.record_monitor_run(&run(10, "manual_ui", "ok")).unwrap();
        d.0.lock().unwrap().execute_batch("PRAGMA user_version = 5").unwrap();
        drop(d);

        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 5, "never lowered");
        assert_eq!(d.monitor_runs(10).len(), 1);
        // Even a run of the migration itself leaves the number alone.
        migrate_v3(&d.0.lock().unwrap()).unwrap();
        assert_eq!(user_version(&d), 5);
    }

    #[test]
    fn opening_a_migrated_file_twice_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        let schema = SCHEMA_V2_MONITOR.replace("user_version = 2", "user_version = 3");
        rusqlite::Connection::open(&path).unwrap().execute_batch(&schema).unwrap();
        let dump = |d: &Db| -> Vec<(String, Option<String>)> {
            let conn = d.0.lock().unwrap();
            let mut stmt =
                conn.prepare("SELECT name, sql FROM sqlite_master ORDER BY name").unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };

        let d = Db::open(&path).unwrap();
        let (schema_once, alerts_once) = (dump(&d), d.alert_rows(true));
        drop(d);
        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 4, "on through v3 to v4");
        assert_eq!(dump(&d), schema_once);
        assert_eq!(d.alert_rows(true), alerts_once);
    }

    #[test]
    fn a_failed_migration_is_kept_for_the_ui_and_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        // A legacy alert table without `at`: the copy into the v3 table cannot run.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE playlist_alert (playlist_id TEXT, video_id TEXT, kind TEXT);
                 PRAGMA user_version = 2;",
            )
            .unwrap();

        let d = Db::open(&path).unwrap();
        let err = d.migration_error().expect("the failure is kept");
        assert!(err.contains("schema v3 migration failed"), "{err}");
        assert_eq!(user_version(&d), 2, "rolled back");
        assert!(!v3_complete(&d.0.lock().unwrap()));
    }

    #[test]
    fn v3_tables_refuse_values_outside_their_checks() {
        let d = db();
        assert_eq!(user_version(&d), 4, "a fresh file is created at v4");
        let alert = |kind| NewAlert {
            playlist_id: "VL1",
            video_id: "a",
            kind,
            song_json: None,
            at: 1,
            from_pos: None,
            to_pos: None,
            dedupe_key: kind,
        };
        assert!(d.insert_alert(&alert("gone")).is_none(), "`gone` is retired");
        assert!(d.insert_alert(&alert("bogus")).is_none());
        assert!(d.insert_alert(&alert("moved")).is_some());
        assert!(d.record_monitor_run(&run(1, "cron", "ok")).is_err());
        assert!(d.record_monitor_run(&run(1, "scheduler", "maybe")).is_err());
        assert!(d.record_monitor_run(&run(1, "headless", "lock_busy")).is_ok());
        let video = VideoMeta { video_id: "a", title: None, channel: None, duration_s: None };
        let flac = NewDownload {
            video,
            format: "flac",
            requested_quality: "best",
            thumbnail_mode: "embed",
            dest_dir: "/m",
            redownload: false,
        };
        assert!(d.enqueue_download(&flac, 1).is_err());
        assert!(d.downloads_for(&["a".into()]).is_empty(), "nothing half-written");
        // `downloads.video_id` has to name a known video.
        let conn = d.0.lock().unwrap();
        let orphan = conn.execute(
            "INSERT INTO downloads(video_id, format, status, requested_quality, dest_dir, \
             created_at) VALUES('nobody', 'audio', 'queued', 'best', '/m', 'x')",
            [],
        );
        assert!(orphan.is_err());
    }

    #[test]
    fn alerts_dedupe_by_key_and_track_what_was_seen() {
        let d = db();
        let key = |scope| alert_dedupe_key("VL1", "a", "removed", scope);
        assert_eq!(key(None), "VL1\u{1f}a\u{1f}removed");
        let file = |key: &str, at| {
            d.insert_alert(&NewAlert {
                playlist_id: "VL1",
                video_id: "a",
                kind: "removed",
                song_json: None,
                at,
                from_pos: None,
                to_pos: None,
                dedupe_key: key,
            })
        };
        let first = file(&key(Some("snap-1")), 1).unwrap();
        assert_eq!(file(&key(Some("snap-1")), 2), None, "the same event twice is one alert");
        let again = file(&key(Some("snap-7")), 3).unwrap();
        assert_ne!(first, again, "the same change in a later snapshot is a new alert");
        assert_eq!(d.unseen_alert_count(), 2);

        assert_eq!(d.mark_alerts_seen(Some(&[first][..])), 1);
        assert_eq!(d.unseen_alert_count(), 1);
        assert_eq!(d.mark_alerts_seen(Some(&[] as &[i64])), 0);
        d.mark_alerts_seen(None);
        assert_eq!(d.unseen_alert_count(), 0);
        assert!(d.alert_rows(false).iter().all(|a| a.seen));

        // Dismissing takes every row of that alert out of the list, and keeps them.
        d.dismiss_playlist_alert("VL1", "a", "removed");
        assert!(d.alert_rows(false).is_empty());
        assert_eq!(d.alert_rows(true).len(), 2);
    }

    #[test]
    fn alert_pages_follow_the_at_and_id_cursor() {
        let d = db();
        // Three rows share an `at`, as one monitor run files them.
        for (at, video) in [(10, "a"), (20, "b"), (20, "c"), (20, "d"), (30, "e")] {
            let key = alert_dedupe_key("VL1", video, "added", None);
            d.insert_alert(&NewAlert {
                playlist_id: "VL1",
                video_id: video,
                kind: "added",
                song_json: None,
                at,
                from_pos: None,
                to_pos: None,
                dedupe_key: &key,
            })
            .unwrap();
        }
        d.dismiss_playlist_alert("VL1", "d", "added");
        let all = d.alert_rows(true);
        assert_eq!(d.alert_rows_page(true, None, None).len(), all.len(), "no limit, no cursor");

        let mut paged = Vec::new();
        let mut cursor = None;
        loop {
            let page = d.alert_rows_page(true, Some(2), cursor);
            assert!(page.len() <= 2);
            let Some(last) = page.last() else { break };
            cursor = Some((last.at, last.id));
            paged.extend(page.into_iter().map(|a| a.id));
        }
        assert_eq!(paged, all.iter().map(|a| a.id).collect::<Vec<_>>(), "no skips, no repeats");

        let shown: Vec<String> =
            d.alert_rows_page(false, Some(10), None).into_iter().map(|a| a.video_id).collect();
        assert_eq!(shown, ["e", "c", "b", "a"], "dismissed ones only when asked for");
    }

    #[test]
    fn only_a_later_restored_resolves_an_unavailable_alert() {
        let d = db();
        let file = |playlist: &str, video: &str, kind: &str, at: i64| {
            let key = alert_dedupe_key(playlist, video, kind, Some(&at.to_string()));
            d.insert_alert(&NewAlert {
                playlist_id: playlist,
                video_id: video,
                kind,
                song_json: None,
                at,
                from_pos: None,
                to_pos: None,
                dedupe_key: &key,
            })
            .unwrap()
        };
        let back = file("VL1", "a", "unavailable", 10); // then restored: resolved
        file("VL1", "a", "restored", 20);
        let before = file("VL1", "b", "restored", 10); // restored before it: still open
        let open = file("VL1", "b", "unavailable", 20);
        let tie = file("VL1", "c", "unavailable", 30); // same `at`, filed first: resolved
        file("VL1", "c", "restored", 30);
        let elsewhere = file("VL2", "a", "unavailable", 15); // another playlist's restore
        let resolved = |rows: Vec<AlertRow>| -> Vec<i64> {
            rows.into_iter().filter(|a| a.resolved).map(|a| a.id).collect()
        };
        let want = vec![tie, back]; // newest first
        assert_eq!(resolved(d.alert_rows(true)), want);
        assert_eq!(resolved(d.alert_rows_page(true, Some(100), None)), want);
        for id in [before, open, elsewhere] {
            assert!(!want.contains(&id));
        }
        // The track going again opens a new alert, which the old restore does not answer.
        file("VL1", "a", "unavailable", 40);
        assert_eq!(resolved(d.alert_rows(true)), want);

        let conn = d.0.lock().unwrap();
        let index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
                 AND name = 'playlist_alert_pvk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn first_seen_survives_rewrites_and_starts_with_the_second_sync() {
        let d = db();
        let songs = |ids: &[&str]| -> Vec<(String, String)> {
            ids.iter().map(|v| (v.to_string(), "{}".to_string())).collect()
        };
        d.set_playlist_songs_at("VL1", &songs(&["a", "b"]), 100);
        assert_eq!(first_seen(&d, "VL1", "a"), None, "a first read cannot date anything");
        let sync = PlaylistSync {
            synced_at: 1,
            item_count: 2,
            added: 0,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        d.set_playlist_sync("VL1", &sync).unwrap();

        d.set_playlist_songs_at("VL1", &songs(&["b", "c"]), 200);
        assert_eq!(first_seen(&d, "VL1", "b"), None, "still the pre-tracking row");
        assert_eq!(first_seen(&d, "VL1", "c"), Some(200));
        d.set_playlist_songs_at("VL1", &songs(&["c", "b"]), 300);
        assert_eq!(first_seen(&d, "VL1", "c"), Some(200), "a rewrite keeps the date");
        d.set_playlist_tracks("VL1", &["c".into()]);
        assert_eq!(first_seen(&d, "VL1", "c"), Some(200));
        d.put_playlist_song("VL1", "c", "{\"t\":1}");
        assert_eq!(first_seen(&d, "VL1", "c"), Some(200), "so does an upsert");

        // Adds made in the app are dated now.
        d.add_playlist_track("VL1", "d");
        d.put_playlist_song("VL2", "e", "{}");
        assert!(first_seen(&d, "VL1", "d").is_some());
        assert!(first_seen(&d, "VL2", "e").is_some());
    }

    #[test]
    fn a_short_read_upserts_without_dropping_the_unread_tail() {
        let d = db();
        let songs = |ids: &[&str], json: &str| -> Vec<(String, String)> {
            ids.iter().map(|v| (v.to_string(), json.to_string())).collect()
        };
        d.set_playlist_songs_at("VL1", &songs(&["a", "b", "c"], "{}"), 100);
        let sync = PlaylistSync {
            synced_at: 100,
            item_count: 3,
            added: 0,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        d.set_playlist_sync("VL1", &sync).unwrap();
        d.set_playlist_songs_at("VL1", &songs(&["a", "b", "c"], "{}"), 150);
        // Cut short after the first page: a re-read and a newcomer, nothing taken away.
        d.upsert_playlist_songs_at("VL1", &songs(&["a", "n"], "{\"t\":2}"), 200);
        let mut got: Vec<(String, Option<String>)> = d.playlist_songs("VL1");
        got.sort();
        let ids: Vec<&str> = got.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "n"]);
        assert_eq!(got[0].1.as_deref(), Some("{\"t\":2}"), "metadata refreshed");
        assert_eq!(first_seen(&d, "VL1", "a"), None, "kept, not re-dated");
        assert_eq!(first_seen(&d, "VL1", "n"), Some(200));
        // The next complete read finds the tail where it was: nothing in it is new.
        d.set_playlist_songs_at("VL1", &songs(&["a", "b", "c", "n"], "{}"), 300);
        assert_eq!(first_seen(&d, "VL1", "c"), None);
        assert_eq!(first_seen(&d, "VL1", "n"), Some(200));
        // Never synced: a short first read dates nothing either.
        d.upsert_playlist_songs_at("VL9", &songs(&["z"], "{}"), 400);
        assert_eq!(first_seen(&d, "VL9", "z"), None);
    }

    #[test]
    fn a_repeat_folds_into_its_monitor_run() {
        let d = db();
        let run = MonitorRun {
            id: 0,
            started_at: 10,
            finished_at: 11,
            trigger: "scheduler".into(),
            outcome: "failed".into(),
            playlists_ok: 0,
            playlists_failed: 0,
            alerts_new: 0,
            units_spent: 0,
            detail_json: "{}".into(),
        };
        let id = d.record_monitor_run(&run).unwrap();
        d.update_monitor_run_repeat(id, 99, "{\"repeats\":1}").unwrap();
        let got = d.monitor_runs(10);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].started_at, got[0].finished_at), (10, 99));
        assert_eq!(got[0].detail_json, "{\"repeats\":1}");
    }

    #[test]
    fn snapshots_are_kept_only_on_change_and_pruned_to_the_newest() {
        let d = db();
        assert_eq!(snapshot_hash(&[]), "d41d8cd98f00b204e9800998ecf8427e", "MD5 of nothing");
        let first = [snap("a"), snap("b")];
        let one = d.put_snapshot_if_changed("VL1", Some("acc"), Some("Mix"), 10, &first).unwrap();
        assert_eq!(
            d.put_snapshot_if_changed("VL1", Some("acc"), Some("Renamed"), 11, &first),
            None,
            "same content, no snapshot"
        );
        let mut art = first.clone();
        art[0].th = Some("https://x/art.jpg".into());
        art[1].a = "Someone else".into();
        assert_eq!(d.put_snapshot_if_changed("VL1", None, None, 12, &art), None, "not content");
        let moved = [snap("b"), snap("a")];
        let two = d.put_snapshot_if_changed("VL1", None, None, 13, &moved).unwrap();
        let mut greyed = snap("a");
        greyed.u = true;
        let greyed_out = [snap("b"), greyed];
        let three = d.put_snapshot_if_changed("VL1", None, None, 14, &greyed_out).unwrap();
        let other = d.put_snapshot_if_changed("VL2", None, None, 14, &[snap("z")]).unwrap();

        let latest = d.latest_snapshot("VL1").unwrap();
        assert_eq!((latest.id, latest.item_count, latest.taken_at), (three, 2, 14));
        assert!(latest.items[1].u);
        assert_eq!(latest.hash, snapshot_hash(&latest.items), "items round-trip");
        let ids = |p: &str| d.snapshots(p).iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids("VL1"), [three, two, one]);
        assert_eq!(d.snapshot(one).unwrap().title.as_deref(), Some("Mix"));

        // Asking for fewer than two still keeps the current one and its reference.
        assert_eq!(d.prune_snapshots(0), [one]);
        assert_eq!(ids("VL1"), [three, two]);
        assert_eq!(ids("VL2"), [other]);
        assert!(d.prune_snapshots(30).is_empty());
        assert!(d.snapshot(one).is_none());
    }

    #[test]
    fn snapshots_outlive_forget_retain_and_sign_out_but_sync_records_do_not() {
        let d = db();
        let sync = PlaylistSync {
            synced_at: 1,
            item_count: 1,
            added: 1,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        for pl in ["VL1", "VL2", "VL3"] {
            d.put_snapshot_if_changed(pl, Some("acc"), None, 1, &[snap("a")]).unwrap();
            d.set_playlist_sync(pl, &sync).unwrap();
        }
        d.record_monitor_run(&run(1, "scheduler", "ok")).unwrap();

        d.forget_playlist("VL1");
        assert!(!d.playlist_syncs().contains_key("VL1"));
        d.retain_playlists(&["VL3".into()]);
        assert_eq!(d.playlist_syncs().into_keys().collect::<Vec<_>>(), ["VL3"]);
        assert_eq!(d.playlist_syncs()["VL3"], sync);
        d.clear_playlist_index();
        assert!(d.playlist_syncs().is_empty());

        for pl in ["VL1", "VL2", "VL3"] {
            assert_eq!(d.snapshots(pl).len(), 1, "{pl}'s history was dropped");
        }
        assert_eq!(d.monitor_runs(10).len(), 1);
    }

    #[test]
    fn snapshot_playlist_ids_list_each_playlist_once_synced_or_not() {
        let d = db();
        assert!(d.snapshot_playlist_ids().is_empty());
        d.put_snapshot_if_changed("VL2", None, None, 1, &[snap("a")]).unwrap();
        d.put_snapshot_if_changed("VL2", None, None, 2, &[snap("b")]).unwrap();
        d.put_snapshot_if_changed("VL1", None, None, 1, &[snap("a")]).unwrap();
        let sync = PlaylistSync {
            synced_at: 1,
            item_count: 1,
            added: 0,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        d.set_playlist_sync("VL1", &sync).unwrap();
        d.set_playlist_sync("VL3", &sync).unwrap();
        d.forget_playlist("VL1");
        assert_eq!(d.snapshot_playlist_ids(), ["VL1", "VL2"], "no snapshot, no VL3");
    }

    #[test]
    fn monitor_runs_come_back_newest_first() {
        let d = db();
        for at in [10, 30, 20] {
            d.record_monitor_run(&run(at, "manual_ui", "partial")).unwrap();
        }
        let got = d.monitor_runs(2);
        assert_eq!(got.iter().map(|r| r.started_at).collect::<Vec<_>>(), [30, 20]);
        assert_eq!((got[0].finished_at, got[0].trigger.as_str()), (35, "manual_ui"));
    }

    #[test]
    fn monitor_stats_count_the_index_and_estimate_duplicates_like_playlistforge() {
        let d = db();
        assert_eq!(d.monitor_stats(), MonitorStats::default(), "an empty file is all zeros");

        let grey = r#"{"video_id":"a","title":"A","artists":"","unavailable":true}"#;
        d.set_playlist_songs("VL1", &[("a".into(), grey.into()), ("b".into(), "{}".into())]);
        d.set_playlist_songs("VL2", &[("a".into(), "{}".into())]);
        let sync = PlaylistSync {
            synced_at: 1,
            item_count: 0,
            added: 0,
            removed: 0,
            moved: 0,
            privacy: None,
        };
        d.set_playlist_sync("VL1", &sync).unwrap();
        // Synced, but empty: no index rows, still a playlist.
        d.set_playlist_sync("VL3", &sync).unwrap();

        // Only the newest snapshot counts: two extra copies there, three in the one before.
        let older = [snap("a"), snap("a"), snap("a"), snap("a"), snap("b")];
        d.put_snapshot_if_changed("VL1", None, None, 1, &older).unwrap();
        let newest = [snap("a"), snap("a"), snap("b"), snap("b")];
        d.put_snapshot_if_changed("VL1", None, None, 2, &newest).unwrap();
        // The same video once in each of two playlists is no duplicate.
        d.put_snapshot_if_changed("VL3", None, None, 2, &[snap("a")]).unwrap();
        // A playlist with no sync record (forgotten, or not synced since v3) is left out.
        d.put_snapshot_if_changed("VL9", None, None, 2, &[snap("z"), snap("z")]).unwrap();

        let stats = d.monitor_stats();
        assert_eq!(
            stats,
            MonitorStats { playlists: 3, items: 3, unavailable: 1, duplicates_estimate: 2 }
        );
    }

    #[test]
    fn alerts_since_lists_kinds_oldest_first_dismissed_included() {
        let d = db();
        for (at, video, kind) in [(20, "c", "removed"), (5, "a", "added"), (10, "b", "moved")] {
            let key = alert_dedupe_key("VL1", video, kind, None);
            d.insert_alert(&NewAlert {
                playlist_id: "VL1",
                video_id: video,
                kind,
                song_json: None,
                at,
                from_pos: None,
                to_pos: None,
                dedupe_key: &key,
            })
            .unwrap();
        }
        d.dismiss_playlist_alert("VL1", "c", "removed");
        let got: Vec<(i64, String)> =
            d.alerts_since(10).into_iter().map(|a| (a.at, a.kind)).collect();
        assert_eq!(got, [(10, "moved".to_string()), (20, "removed".to_string())]);
        assert!(d.alerts_since(21).is_empty());
    }

    #[test]
    fn downloads_move_through_their_states() {
        let d = db();
        let req = |video_id, redownload| NewDownload {
            video: VideoMeta {
                video_id,
                title: Some("T"),
                channel: Some("C"),
                duration_s: Some(200),
            },
            format: "audio",
            requested_quality: "best",
            thumbnail_mode: "embed",
            dest_dir: "/music",
            redownload,
        };
        let status = |v: &str| d.downloads_for(&[v.to_owned()])[0].status.clone();

        assert_eq!(d.enqueue_download(&req("a", false), 100).unwrap(), EnqueueOutcome::Queued);
        assert_eq!(d.enqueue_download(&req("b", false), 101).unwrap(), EnqueueOutcome::Queued);
        assert_eq!(d.enqueue_download(&req("a", true), 102).unwrap(), EnqueueOutcome::Already);
        assert_eq!(d.next_queued_download().unwrap().video_id, "a", "first in, first out");

        assert!(d.mark_download_running("a", "audio"));
        assert!(!d.mark_download_running("a", "audio"), "only a queued row starts");
        assert_eq!(d.next_queued_download().unwrap().video_id, "b");
        // A run that died mid-download leaves it running; the runner's start puts it back.
        assert_eq!(d.reset_orphaned_downloads(), 1);
        assert_eq!(status("a"), "queued");
        assert!(d.mark_download_running("a", "audio"));
        let file = "/music/a.m4a";
        assert!(d.mark_download_available("a", "audio", file, Some(1234), Some("m4a"), 150));
        let row = d.downloads_for(&["a".into()]).remove(0);
        assert_eq!((row.status.as_str(), row.attempts), ("available", 2));
        assert_eq!(row.completed_at.as_deref(), Some(rfc3339_utc(150).as_str()));
        assert_eq!((row.file_path.as_deref(), row.file_size_bytes), (Some(file), Some(1234)));

        assert_eq!(d.enqueue_download(&req("a", false), 160).unwrap(), EnqueueOutcome::Already);
        assert_eq!(d.enqueue_download(&req("a", true), 161).unwrap(), EnqueueOutcome::Requeued);
        assert_eq!(status("a"), "queued");

        assert!(d.mark_download_error("b", "audio", "HTTP 403"));
        assert_eq!(d.enqueue_download(&req("b", false), 170).unwrap(), EnqueueOutcome::Requeued);
        assert!(d.mark_download_running("b", "audio"));
        assert!(d.mark_download_available("b", "audio", "/music/b.m4a", None, None, 180));
        assert!(d.mark_download_verified("b", "audio", 190));
        assert!(d.mark_download_missing("b", "audio", 200));
        assert!(!d.mark_download_verified("b", "audio", 210), "only an available row verifies");
        assert_eq!(status("b"), "missing");
        assert!(d.requeue_download("b", "audio", 220));
        assert!(!d.requeue_download("b", "audio", 230), "already queued");

        assert_eq!(d.downloads_for(&["a".into(), "b".into(), "zz".into()]).len(), 2);
        assert_eq!(d.recent_downloads(1).len(), 1);
        assert!(d.delete_download("a", "audio"));
        assert!(d.downloads_for(&["a".into()]).is_empty());
        let conn = d.0.lock().unwrap();
        let title: Option<String> = conn
            .query_row("SELECT title FROM videos WHERE video_id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title.as_deref(), Some("T"), "the video row stays");
    }

    #[test]
    fn rfc3339_utc_formats_unix_seconds() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert!(rfc3339_utc(99) < rfc3339_utc(1_000), "sorts as text in time order");
    }

    // --- schema v4 ------------------------------------------------------------------------------

    /// A v3 file exactly as the v3 code leaves it: the v2 monitor tables run through
    /// `migrate_v3`, with a synced playlist. `Db::open` creates everything else.
    fn v3_file(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V2_MONITOR).unwrap();
        migrate_v3(&conn).unwrap();
        conn.execute(
            "INSERT INTO playlist_sync(playlist_id, synced_at, item_count) VALUES('VL1', 5, 1)",
            [],
        )
        .unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 3);
        assert!(v3_complete(&conn));
        assert!(!v4_complete(&conn));
    }

    fn schema_dump(d: &Db) -> Vec<(String, Option<String>)> {
        let conn = d.conn();
        let mut stmt = conn.prepare("SELECT name, sql FROM sqlite_master ORDER BY name").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    fn count(d: &Db, sql: &str) -> i64 {
        d.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_v3_database_migrates_to_v4_once_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        v3_file(&path);

        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 4);
        assert!(v4_complete(&d.conn()));
        assert_eq!(count(&d, "SELECT COUNT(*) FROM playlist_track"), 1);
        assert_eq!(
            count(&d, "SELECT COUNT(*) FROM playlist_track WHERE added_at IS NULL"),
            1,
            "a track indexed before v4 has no known date"
        );
        assert_eq!(count(&d, "SELECT COUNT(*) FROM playlist_sync WHERE privacy IS NULL"), 1);
        assert_eq!(d.alert_rows(true).len(), 3, "v3's alerts are untouched");
        let once = schema_dump(&d);
        drop(d);

        // Opening again changes nothing, and neither does running the whole migration again.
        let d = Db::open(&path).unwrap();
        assert_eq!(schema_dump(&d), once);
        d.conn().execute_batch("PRAGMA user_version = 3").unwrap();
        drop(d);
        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 4);
        assert_eq!(schema_dump(&d), once);
        assert_eq!(count(&d, "SELECT COUNT(*) FROM playlist_sync"), 1);
    }

    #[test]
    fn a_file_numbered_4_without_the_v4_tables_still_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        v3_file(&path);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch("PRAGMA user_version = 4").unwrap();
        drop(raw);

        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 4);
        assert!(v4_complete(&d.conn()), "the artefact check, not the number, decides");
    }

    #[test]
    fn a_complete_v4_file_numbered_past_4_keeps_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        let d = Db::open(&path).unwrap();
        d.conn()
            .execute(
                "INSERT INTO quota_ledger(ts, endpoint, units) VALUES('2026-07-15T18:00:00+00:00', \
                 'playlists.list', 1)",
                [],
            )
            .unwrap();
        d.conn().execute_batch("PRAGMA user_version = 7").unwrap();
        drop(d);

        let d = Db::open(&path).unwrap();
        assert_eq!(d.migration_error(), None);
        assert_eq!(user_version(&d), 7, "never lowered");
        assert_eq!(count(&d, "SELECT COUNT(*) FROM quota_ledger"), 1);
        migrate_v4(&d.conn()).unwrap();
        assert_eq!(user_version(&d), 7, "not even by the migration itself");
    }

    #[test]
    fn a_failed_v4_migration_is_kept_for_the_ui_and_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limusic.sqlite");
        v3_file(&path);
        // Something else's `jobs` table: v4's index on it cannot be built.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE jobs (id INTEGER PRIMARY KEY)")
            .unwrap();

        let d = Db::open(&path).unwrap();
        let err = d.migration_error().expect("the failure is kept");
        assert!(err.contains("schema v4 migration failed"), "{err}");
        assert_eq!(user_version(&d), 3, "rolled back");
        let conn = d.conn();
        assert!(!has_column(&conn, "playlist_track", "added_at").unwrap(), "the ALTER too");
        assert!(!has_table(&conn, "quota_ledger"));
        assert!(v3_complete(&conn), "v3 is left as it was");
    }

    #[test]
    fn v4_foreign_keys_cascade_items_and_unlink_jobs_and_ledger() {
        let d = db();
        let conn = d.conn();
        let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1, "foreign keys are enforced on this connection");
        conn.execute_batch(
            "INSERT INTO ytdata_accounts(channel_id, title, added_at) VALUES('UC1', 'Me', 1);
             INSERT INTO jobs(id, account_id, kind, created_at) VALUES(1, 'UC1', 'copy_items', 't');
             INSERT INTO jobs(id, account_id, kind, created_at) VALUES(2, 'UC1', 'copy_items', 't');
             INSERT INTO job_items(job_id, seq, action, updated_at)
                 VALUES(1, 0, 'a', 't'), (1, 1, 'a', 't'), (2, 0, 'a', 't');
             INSERT INTO quota_ledger(ts, endpoint, units, account_id, job_id)
                 VALUES('t', 'playlistItems.insert', 50, 'UC1', 1),
                       ('t', 'playlists.list', 1, 'UC1', NULL);",
        )
        .unwrap();
        let n = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };

        conn.execute("DELETE FROM jobs WHERE id = 1", []).unwrap();
        assert_eq!(n("SELECT COUNT(*) FROM job_items WHERE job_id = 1"), 0, "items go with it");
        assert_eq!(n("SELECT COUNT(*) FROM job_items"), 1);
        assert_eq!(n("SELECT COUNT(*) FROM quota_ledger"), 2, "the spend stays");
        assert_eq!(n("SELECT COUNT(*) FROM quota_ledger WHERE job_id IS NOT NULL"), 0);

        conn.execute("DELETE FROM ytdata_accounts WHERE channel_id = 'UC1'", []).unwrap();
        assert_eq!(n("SELECT COUNT(*) FROM jobs WHERE account_id IS NULL"), 1, "job kept, unowned");
        assert_eq!(
            n("SELECT COUNT(*) FROM quota_ledger WHERE account_id = 'UC1'"),
            2,
            "the ledger keeps naming the account that spent the units"
        );
        let no = |sql: &str| conn.execute(sql, []).is_err();
        assert!(
            no("INSERT INTO jobs(account_id, kind, created_at) VALUES('UC9', 'k', 't')"),
            "a job cannot name an account that is not connected"
        );
        assert!(
            no("INSERT INTO job_items(job_id, seq, action, updated_at) VALUES(9, 0, 'a', 't')"),
            "an item cannot outlive its job"
        );
    }

    #[test]
    fn v4_tables_refuse_values_outside_their_checks() {
        let d = db();
        let conn = d.conn();
        conn.execute(
            "INSERT INTO playlist_sync(playlist_id, synced_at, item_count) VALUES('VL1', 1, 0)",
            [],
        )
        .unwrap();
        for ok in ["public", "unlisted", "private"] {
            assert!(conn.execute("UPDATE playlist_sync SET privacy = ?1", [ok]).is_ok(), "{ok}");
        }
        let no = |sql: &str| conn.execute(sql, []).is_err();
        assert!(no("UPDATE playlist_sync SET privacy = 'secret'"));
        assert!(no("INSERT INTO ytdata_accounts(channel_id, title, status, added_at) \
                    VALUES('UC1', 'Me', 'gone', 1)"));
        assert!(no("INSERT INTO runner_lock(id, pid, heartbeat_at) VALUES(2, 1, 't')"));
    }

    #[test]
    fn added_at_survives_an_index_rewrite_and_starts_unknown() {
        let d = db();
        let song = |v: &str| (v.to_owned(), "{}".to_owned());
        d.set_playlist_songs_at("VL1", &[song("a")], 10);
        d.conn().execute_batch("UPDATE playlist_track SET added_at = 1234").unwrap();
        d.set_playlist_songs_at("VL1", &[song("a"), song("b")], 20);
        let added = |v: &str| -> Option<i64> {
            d.conn()
                .query_row("SELECT added_at FROM playlist_track WHERE video_id = ?1", [v], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(added("a"), Some(1234), "kept across the rewrite");
        assert_eq!(added("b"), None, "a new track's date is unknown until the Data API says");
        // The Data API's dates land on the rows it names; the others keep theirs.
        let dates: std::collections::HashMap<String, i64> =
            [("b".to_owned(), 500), ("zz".to_owned(), 9)].into();
        d.set_playlist_added_at("VL1", &dates);
        assert_eq!((added("a"), added("b")), (Some(1234), Some(500)));
        assert_eq!(d.playlist_songs("VL1").len(), 2, "no row made up for a track not indexed");
        // An InnerTube rewrite (no dates) keeps both.
        d.set_playlist_songs_at("VL1", &[song("a"), song("b")], 30);
        assert_eq!((added("a"), added("b")), (Some(1234), Some(500)));
        // The earliest date per song across playlists, Liked Music aside.
        d.set_playlist_songs_at("VL2", &[song("a")], 30);
        d.set_playlist_added_at("VL2", &[("a".to_owned(), 100)].into());
        d.set_playlist_songs_at("VLLM", &[song("b")], 30);
        d.set_playlist_added_at("VLLM", &[("b".to_owned(), 1)].into());
        let earliest = d.playlist_added_dates();
        assert_eq!((earliest.get("a"), earliest.get("b")), (Some(&100), Some(&500)));
    }

    #[test]
    fn privacy_is_kept_until_the_data_api_says_otherwise() {
        let d = db();
        let sync = |privacy| PlaylistSync {
            synced_at: 1,
            item_count: 0,
            added: 0,
            removed: 0,
            moved: 0,
            privacy,
        };
        d.set_playlist_sync("VL1", &sync(None)).unwrap();
        assert_eq!(d.playlist_syncs()["VL1"].privacy, None, "unknown until read");
        d.set_playlist_sync("VL1", &sync(Some(Privacy::Unlisted))).unwrap();
        d.set_playlist_sync("VL1", &sync(None)).unwrap();
        assert_eq!(d.playlist_syncs()["VL1"].privacy, Some(Privacy::Unlisted), "kept");
        d.set_playlist_sync("VL1", &sync(Some(Privacy::Public))).unwrap();
        assert_eq!(d.playlist_syncs()["VL1"].privacy, Some(Privacy::Public));
        assert_eq!(Privacy::parse("PRIVATE"), Some(Privacy::Private));
        assert_eq!(Privacy::parse("secret"), None);
        let json = serde_json::to_value(d.playlist_syncs()["VL1"]).unwrap();
        assert_eq!(json["privacy"], "public");
    }

    #[test]
    fn recover_titles_round_trip_and_replace() {
        let d = db();
        assert_eq!(d.recover_title_get("dQw4w9WgXcQ"), None, "nothing cached yet");
        let found = CachedTitle {
            title: Some("Never Gonna Give You Up".into()),
            artists: Some("Rick Astley".into()),
            source: "wayback".into(),
            found: true,
            checked_at: 1_000,
        };
        d.recover_title_put("dQw4w9WgXcQ", &found);
        assert_eq!(d.recover_title_get("dQw4w9WgXcQ"), Some(found.clone()));
        // A found title never goes stale, however old.
        assert_eq!(
            d.recover_title_get_at("dQw4w9WgXcQ", 1_000 + 10 * RECOVER_NEGATIVE_TTL_SECS),
            Some(found)
        );
        let replaced = CachedTitle {
            title: Some("Other".into()),
            artists: None,
            source: "manual".into(),
            found: true,
            checked_at: 2_000,
        };
        d.recover_title_put("dQw4w9WgXcQ", &replaced);
        assert_eq!(d.recover_title_get("dQw4w9WgXcQ"), Some(replaced));
    }

    #[test]
    fn recover_titles_negative_verdicts_expire_after_thirty_days() {
        let d = db();
        let now = 100 * RECOVER_NEGATIVE_TTL_SECS;
        let negative = |checked_at| CachedTitle {
            title: None,
            artists: None,
            source: "wayback".into(),
            found: false,
            checked_at,
        };
        d.recover_title_put("aaaaaaaaaaa", &negative(now - 29 * 24 * 60 * 60));
        d.recover_title_put("bbbbbbbbbbb", &negative(now - 31 * 24 * 60 * 60));
        let fresh = d.recover_title_get_at("aaaaaaaaaaa", now).expect("fresh negative is kept");
        assert!(!fresh.found);
        assert_eq!(fresh.title, None);
        assert_eq!(d.recover_title_get_at("bbbbbbbbbbb", now), None, "stale negative is ignored");
    }

    #[test]
    fn the_recovery_reads_snapshots_dead_index_rows_and_stored_titles() {
        let d = db();
        let mut dead = snap("x");
        dead.u = true;
        dead.t = "Deleted video".into();
        d.put_snapshot_if_changed("VL1", None, None, 10, &[snap("a"), snap("x")]).unwrap();
        d.put_snapshot_if_changed("VL1", None, None, 20, &[snap("a"), dead]).unwrap();
        d.put_snapshot_if_changed("VL2", None, None, 15, &[snap("b")]).unwrap();
        let latest = d.latest_snapshots();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["VL1"].taken_at, 20, "the newest per playlist");
        assert_eq!(latest["VL2"].items, vec![snap("b")]);
        assert_eq!(d.snapshot_before("VL1", 20).map(|s| s.taken_at), Some(10), "strictly before");
        assert_eq!(d.snapshot_before("VL1", 10), None);

        let dead_json = r#"{"video_id":"x","title":"","artists":"","unavailable":true}"#;
        d.put_playlist_song("VL3", "x", dead_json);
        d.put_playlist_song("VL3", "a", r#"{"video_id":"a","title":"A","artists":""}"#);
        let index = d.unavailable_index_rows();
        assert_eq!(index, vec![("VL3".to_owned(), "x".to_owned(), dead_json.to_owned())]);

        let key = alert_dedupe_key("VL1", "x", "unavailable", None);
        d.insert_alert(&NewAlert {
            playlist_id: "VL1",
            video_id: "x",
            kind: "unavailable",
            song_json: Some(r#"{"alert":1}"#),
            at: 20,
            from_pos: None,
            to_pos: None,
            dedupe_key: &key,
        })
        .unwrap();
        d.record_play("x", r#"{"play":1}"#, 30, 1_000);
        d.record_play("other", r#"{"play":2}"#, 30, 1_000);
        d.conn()
            .execute(
                "INSERT INTO videos(video_id, title, channel, updated_at) \
                 VALUES('x', 'Vid X', 'Band - Topic', 1)",
                [],
            )
            .unwrap();
        let rows = d.local_title_rows(&["x".to_owned()]);
        let s = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            rows.snapshots,
            vec![("x".to_owned(), "X".to_owned(), "Artist".to_owned())],
            "only the rows where it still played"
        );
        assert_eq!(rows.alerts, vec![s("x", r#"{"alert":1}"#)]);
        assert_eq!(rows.plays, vec![s("x", r#"{"play":1}"#)], "only the ids asked for");
        assert_eq!(rows.videos, vec![("x".into(), "Vid X".into(), Some("Band - Topic".into()))]);
        assert_eq!(rows.index, vec![s("x", dead_json)]);
        assert_eq!(d.local_title_rows(&[]), LocalTitleRows::default());
    }
}

// Queue persistence lives in the `settings` KV as a JSON blob (`queue_json`) + `queue_position`,
// so restore round-trips the full SongItem losslessly via serde (context/11 §state).
