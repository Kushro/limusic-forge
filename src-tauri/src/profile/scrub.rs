//! The database half of a profile: take a consistent snapshot of the live file, then make the
//! copy safe to put in a zip.
//!
//! A `.lmfprofile` must never carry a secret in `profile.sqlite`: anyone can open that with a
//! viewer, and "export without secrets" has to mean it. But the app keeps its secrets *in* that
//! database (Google cookies, the Last.fm session, the ListenBrainz token, a proxy password), so
//! the export lifts them out of the snapshot ([`scrub`]), hands them back as a [`SecretsBundle`]
//! (sealed into `secrets.bin` when the user opted in, dropped otherwise) and the import puts them
//! back ([`restore_secrets`]) or, with no secrets, leaves the app cleanly signed out
//! ([`drop_signed_out_accounts`]).
//!
//! Everything here runs on the SNAPSHOT, never on the live database. [`snapshot`] is the only
//! function that touches the live file, and only through a read-only connection.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::{Counts, GoogleSession, ProfileError, Result, SecretsBundle};

/// Settings that describe THIS machine at THIS moment, not the user's profile: window positions in
/// physical pixels, the browser-free PO-token session, back-off stamps and the monitor's
/// bookkeeping, scan/index freshness marks. Carried over they would be wrong (a mini player off
/// screen) or harmful (a "quota exceeded today" that blocks the Data API, an index stamp that says
/// the playlist index is fresh when this database has none of it on the new machine's schedule).
/// `scrub` deletes them all; the app recreates what it needs.
///
/// Not listed on purpose: `visitor_data`, `data_sync_id`, `selected_identity_json`, `account_json`
/// (identity, not secrets) and `monitor.schedule_time` / `monitor.backups_dir` (preferences; the
/// importer offers to re-register the task and remaps the dir).
pub const MACHINE_KEYS: &[&str] = &[
    "potoken_session",
    "mini_position",
    "mini_dyn_position",
    "ytdata.api_disabled_at",
    "ytdata.quota_exceeded_day",
    "monitor_last_attempt_at",
    "monitor_failures",
    "import_cooldown_until",
    "local_scan_version",
    "playlist_index_synced_at",
    "playlist_index_synced_at_v3",
    "playlist_index_last_summary",
];

/// Settings that identify the signed-in Google account. Cleared together by
/// [`drop_signed_out_accounts`] when no session survives (see there).
const IDENTITY_KEYS: &[&str] = &[
    "session_cookie",
    "selected_identity_json",
    "data_sync_id",
    "account_json",
    "account_selection_pending",
    "active_account",
];

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .optional()?)
}

/// `Some` only when the value is there and not blank (a logout stores `""`).
fn non_empty_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(get_setting(conn, key)?.filter(|v| !v.trim().is_empty()))
}

fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

fn delete_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

/// Empty a table that older schemas may not have yet.
fn clear_table(conn: &Connection, table: &str) -> Result<()> {
    if table_exists(conn, table)? {
        conn.execute(&format!("DELETE FROM {table}"), [])?;
    }
    Ok(())
}

/// Whether a proxy URL carries `user[:pass]@` in its authority (`scheme://user:pass@host:port`).
/// Only then is the whole setting a secret; a plain `http://host:8080` stays in the database
/// like any other preference.
fn proxy_has_credentials(proxy: &str) -> bool {
    let rest = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

/// `VACUUM INTO` from a fresh read-only connection on the live WAL database. The app keeps its own
/// connection open; a second reader is fine in WAL mode and sees a consistent state, and copying
/// the files raw would not be (the WAL holds committed pages the main file lacks). `out` must not
/// exist: SQLite refuses to overwrite.
pub fn snapshot(live_db: &Path, out: &Path) -> Result<()> {
    let target = out
        .to_str()
        .ok_or_else(|| ProfileError::Other("the snapshot path is not valid UTF-8".into()))?;
    let conn = Connection::open_with_flags(
        live_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // A reader waits for a checkpoint in flight instead of failing at once.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute("VACUUM INTO ?1", [target])?;
    Ok(())
}

/// On the SNAPSHOT copy only: drop caches and machine-local state, lift every database-held secret
/// out and return it. After this the file holds no credential, whatever the caller does with the
/// returned bundle.
///
/// What happens to each secret:
/// - `settings.session_cookie`: row deleted, value into `active_session_cookie`;
/// - `accounts.session_cookie` (NOT NULL, so blanked rather than deleted): each non-blank value
///   into `google_sessions` keyed by the account id;
/// - `lastfm_session_key`, `listenbrainz_token`: rows deleted, values returned;
/// - `proxy`: moved (and the row deleted) only when it embeds credentials; otherwise left alone.
///
/// Blank values (a logged-out Last.fm stores `""`) are removed but not reported as present, so the
/// bundle's counts match what the user really has.
///
/// `include_caches = false` also empties `lyrics_cache`; with `true` it is kept (it is slow to
/// rebuild and harmless). `stream_url_cache` is always emptied: those URLs are signed and expire
/// within hours.
pub fn scrub(conn: &Connection, include_caches: bool) -> Result<SecretsBundle> {
    // Deleting a row only unlinks it: the bytes would sit in the file's free pages, readable with
    // `strings`. `secure_delete` zeroes them as they are freed or overwritten.
    conn.pragma_update(None, "secure_delete", "ON")?;
    let tx = conn.unchecked_transaction()?;
    let mut bundle = SecretsBundle::default();

    bundle.active_session_cookie = non_empty_setting(&tx, "session_cookie")?;
    delete_setting(&tx, "session_cookie")?;

    if table_exists(&tx, "accounts")? {
        {
            let mut stmt = tx.prepare("SELECT id, session_cookie FROM accounts ORDER BY id")?;
            let rows = stmt.query_map([], |r| {
                Ok(GoogleSession { account_id: r.get(0)?, session_cookie: r.get(1)? })
            })?;
            for row in rows {
                let s = row?;
                if !s.session_cookie.trim().is_empty() {
                    bundle.google_sessions.push(s);
                }
            }
        }
        tx.execute("UPDATE accounts SET session_cookie = ''", [])?;
    }

    bundle.lastfm_session_key = non_empty_setting(&tx, "lastfm_session_key")?;
    delete_setting(&tx, "lastfm_session_key")?;
    bundle.listenbrainz_token = non_empty_setting(&tx, "listenbrainz_token")?;
    delete_setting(&tx, "listenbrainz_token")?;

    if let Some(proxy) = non_empty_setting(&tx, "proxy")? {
        if proxy_has_credentials(&proxy) {
            bundle.proxy = Some(proxy);
            delete_setting(&tx, "proxy")?;
        }
    }

    for key in MACHINE_KEYS {
        delete_setting(&tx, key)?;
    }
    clear_table(&tx, "stream_url_cache")?;
    clear_table(&tx, "runner_lock")?;
    if !include_caches {
        clear_table(&tx, "lyrics_cache")?;
    }

    tx.commit()?;
    Ok(bundle)
}

/// Write the database-held fields of `bundle` back (import side, on the staged copy). Accounts
/// are matched by id; a session for an account the snapshot does not have is ignored (there is no
/// row to give it an identity). Fields the bundle lacks are left untouched.
pub fn restore_secrets(conn: &Connection, bundle: &SecretsBundle) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if let Some(cookie) = &bundle.active_session_cookie {
        set_setting(&tx, "session_cookie", cookie)?;
    }
    if table_exists(&tx, "accounts")? {
        for s in &bundle.google_sessions {
            tx.execute(
                "UPDATE accounts SET session_cookie = ?1 WHERE id = ?2",
                [&s.session_cookie, &s.account_id],
            )?;
        }
    }
    if let Some(v) = &bundle.lastfm_session_key {
        set_setting(&tx, "lastfm_session_key", v)?;
    }
    if let Some(v) = &bundle.listenbrainz_token {
        set_setting(&tx, "listenbrainz_token", v)?;
    }
    if let Some(v) = &bundle.proxy {
        set_setting(&tx, "proxy", v)?;
    }
    tx.commit()?;
    Ok(())
}

/// Import without (all) secrets: leave the app signed out in a state it boots from without
/// errors. Read from db.rs: the app is "signed in" when `settings.session_cookie` is non-empty;
/// the identity (`selected_identity_json`, `data_sync_id`, `account_json`) and `active_account`
/// are projections of one `accounts` row, and `account_selection_pending` forces the channel
/// picker for a cookie. A cookie-less account row would list in the account switcher and fail on
/// click, and identity left without a cookie would show a name next to a signed-out session.
///
/// So: accounts with a blank cookie are deleted; then, unless a usable active session remains
/// (non-blank `session_cookie` setting), every [`IDENTITY_KEYS`] setting is cleared and the
/// playlist membership index (`playlist_track`, which belongs to one account) and its freshness
/// stamps go with it, exactly like a sign-out does (`AppState::forget_playlist_index`).
/// `active_account` is also dropped when it points at an account that no longer exists.
/// `visitor_data` is device-agnostic and kept. Data API channels linked to a deleted account are
/// unlinked, not removed: they authenticate with their own refresh token.
pub fn drop_signed_out_accounts(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if table_exists(&tx, "accounts")? {
        tx.execute("DELETE FROM accounts WHERE trim(session_cookie) = ''", [])?;
    }

    if non_empty_setting(&tx, "session_cookie")?.is_none() {
        for key in IDENTITY_KEYS {
            delete_setting(&tx, key)?;
        }
        clear_table(&tx, "playlist_track")?;
        delete_setting(&tx, "playlist_index_synced_at")?;
        delete_setting(&tx, "playlist_index_synced_at_v3")?;
        delete_setting(&tx, "playlist_index_last_summary")?;
        delete_setting(&tx, "monitor_failures")?;
    } else if let Some(active) = get_setting(&tx, "active_account")? {
        let exists = table_exists(&tx, "accounts")?
            && tx
                .query_row("SELECT 1 FROM accounts WHERE id = ?1", [&active], |_| Ok(()))
                .optional()?
                .is_some();
        if !exists {
            delete_setting(&tx, "active_account")?;
        }
    }

    if table_exists(&tx, "ytdata_accounts")? {
        let accounts = table_exists(&tx, "accounts")?;
        let sql = if accounts {
            "UPDATE ytdata_accounts SET linked_account = NULL
             WHERE linked_account IS NOT NULL AND linked_account NOT IN (SELECT id FROM accounts)"
        } else {
            "UPDATE ytdata_accounts SET linked_account = NULL WHERE linked_account IS NOT NULL"
        };
        // `linked_account` was added by a later migration; an older file simply lacks it.
        let _ = tx.execute(sql, []);
    }
    tx.commit()?;
    Ok(())
}

fn count(conn: &Connection, table: &str) -> Result<u64> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
    Ok(n.max(0) as u64)
}

/// Row counts for the manifest and the import preview. A table missing from an older schema is 0.
/// `files` stays 0: export.rs counts what it copies.
pub fn counts(conn: &Connection) -> Result<Counts> {
    Ok(Counts {
        settings: count(conn, "settings")?,
        local_tracks: count(conn, "local_tracks")?,
        local_playlists: count(conn, "local_playlists")?,
        listens: count(conn, "listens")?,
        accounts: count(conn, "accounts")?,
        ytdata_accounts: count(conn, "ytdata_accounts")?,
        snapshots: count(conn, "playlist_snapshot")?,
        downloads: count(conn, "downloads")?,
        files: 0,
    })
}

/// Channel ids of the connected Data API accounts, oldest first: the refresh tokens to collect.
pub fn ytdata_channel_ids(conn: &Connection) -> Result<Vec<String>> {
    if !table_exists(conn, "ytdata_accounts")? {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT channel_id FROM ytdata_accounts ORDER BY added_at, channel_id")?;
    let ids =
        stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real schema (every migration) with the secrets and machine state a used install has.
    fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("live.sqlite");
        drop(crate::db::Db::open(&path).expect("open db"));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO settings(key, value) VALUES
                ('session_cookie', 'SAPISID=active-secret'),
                ('selected_identity_json', '{"name":"A"}'),
                ('data_sync_id', 'dsid1'),
                ('account_json', '{"n":"A"}'),
                ('active_account', 'acc1'),
                ('visitor_data', 'visitor-keep'),
                ('lastfm_session_key', 'lfm-secret'),
                ('listenbrainz_token', 'lb-secret'),
                ('proxy', 'http://user:hunter2@proxy.local:3128'),
                ('potoken_session', '{"x":1}'),
                ('mini_position', '10,20'),
                ('mini_dyn_position', '30,40'),
                ('ytdata.api_disabled_at', '123'),
                ('ytdata.quota_exceeded_day', '2026-01-01'),
                ('monitor_last_attempt_at', '5'),
                ('monitor_failures', '2'),
                ('import_cooldown_until', '9'),
                ('local_scan_version', '3'),
                ('playlist_index_synced_at_v3', '77'),
                ('playlist_index_last_summary', '{}'),
                ('theme', 'dark');
            INSERT INTO accounts(id, session_cookie, data_sync_id, added_at) VALUES
                ('acc1', 'cookie-one', 'd1', 1),
                ('acc2', 'cookie-two', 'd2', 2);
            INSERT INTO stream_url_cache(video_id, url, itag, expires_at) VALUES('v', 'u', 1, 2);
            INSERT INTO lyrics_cache(video_id, lyrics, fetched_at) VALUES('v', 'la', 1);
            INSERT INTO runner_lock(id, pid, heartbeat_at) VALUES(1, 42, 't');
            "#,
        )
        .unwrap();
        path
    }

    fn setting(conn: &Connection, key: &str) -> Option<String> {
        get_setting(conn, key).unwrap()
    }

    fn n(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn profile_scrub_snapshot_copies_live_wal_db_and_refuses_existing_out() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        // Keep a writer open so the data may still sit in the WAL, like the running app.
        let writer = Connection::open(&live).unwrap();
        writer.execute("INSERT INTO settings(key, value) VALUES('late', 'row')", []).unwrap();

        let out = dir.path().join("snap.sqlite");
        snapshot(&live, &out).unwrap();
        let snap = Connection::open(&out).unwrap();
        assert_eq!(setting(&snap, "late").as_deref(), Some("row"));
        assert_eq!(setting(&snap, "theme").as_deref(), Some("dark"));
        assert!(snapshot(&live, &out).is_err(), "VACUUM INTO must not overwrite");
        drop(writer);
    }

    #[test]
    fn profile_scrub_lifts_every_secret_out_of_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let out = dir.path().join("snap.sqlite");
        snapshot(&live, &out).unwrap();
        let conn = Connection::open(&out).unwrap();

        let bundle = scrub(&conn, true).unwrap();
        assert_eq!(bundle.active_session_cookie.as_deref(), Some("SAPISID=active-secret"));
        assert_eq!(bundle.google_sessions.len(), 2);
        assert_eq!(bundle.google_sessions[0].account_id, "acc1");
        assert_eq!(bundle.google_sessions[1].session_cookie, "cookie-two");
        assert_eq!(bundle.lastfm_session_key.as_deref(), Some("lfm-secret"));
        assert_eq!(bundle.listenbrainz_token.as_deref(), Some("lb-secret"));
        assert_eq!(bundle.proxy.as_deref(), Some("http://user:hunter2@proxy.local:3128"));

        for key in ["session_cookie", "lastfm_session_key", "listenbrainz_token", "proxy"] {
            assert_eq!(setting(&conn, key), None, "{key}");
        }
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM accounts WHERE session_cookie <> ''"), 0);
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM accounts"), 2, "rows stay, only blanked");

        // Nothing secret survives anywhere in the file's text.
        drop(conn);
        let raw = std::fs::read(&out).unwrap();
        let hay = String::from_utf8_lossy(&raw);
        for needle in
            ["active-secret", "cookie-one", "cookie-two", "lfm-secret", "lb-secret", "hunter2"]
        {
            assert!(!hay.contains(needle), "{needle} leaked");
        }
    }

    #[test]
    fn profile_scrub_drops_machine_state_and_keeps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        scrub(&conn, true).unwrap();

        for key in MACHINE_KEYS {
            assert_eq!(setting(&conn, key), None, "{key}");
        }
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM stream_url_cache"), 0);
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM runner_lock"), 0);
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM lyrics_cache"), 1, "kept with include_caches");
        assert_eq!(setting(&conn, "visitor_data").as_deref(), Some("visitor-keep"));
        assert_eq!(setting(&conn, "data_sync_id").as_deref(), Some("dsid1"));
        assert_eq!(setting(&conn, "selected_identity_json").as_deref(), Some(r#"{"name":"A"}"#));
        assert_eq!(setting(&conn, "theme").as_deref(), Some("dark"));
    }

    #[test]
    fn profile_scrub_empties_lyrics_cache_without_caches() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        scrub(&conn, false).unwrap();
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM lyrics_cache"), 0);
    }

    #[test]
    fn profile_scrub_leaves_a_proxy_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        set_setting(&conn, "proxy", "socks5://proxy.local:1080/path@odd").unwrap();
        let bundle = scrub(&conn, true).unwrap();
        assert_eq!(bundle.proxy, None);
        assert_eq!(setting(&conn, "proxy").as_deref(), Some("socks5://proxy.local:1080/path@odd"));
        assert!(proxy_has_credentials("http://u@h:1"));
        assert!(proxy_has_credentials("u:p@h:1"));
        assert!(!proxy_has_credentials("http://h:1"));
    }

    #[test]
    fn profile_scrub_blank_values_are_not_counted_as_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        set_setting(&conn, "lastfm_session_key", "").unwrap();
        set_setting(&conn, "session_cookie", "  ").unwrap();
        conn.execute("UPDATE accounts SET session_cookie = '' WHERE id = 'acc2'", []).unwrap();
        let bundle = scrub(&conn, true).unwrap();
        assert_eq!(bundle.lastfm_session_key, None);
        assert_eq!(bundle.active_session_cookie, None);
        assert_eq!(bundle.google_sessions.len(), 1);
        assert_eq!(setting(&conn, "lastfm_session_key"), None);
    }

    #[test]
    fn profile_scrub_restore_round_trips_and_ignores_unknown_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        let mut bundle = scrub(&conn, true).unwrap();
        bundle.google_sessions.push(GoogleSession {
            account_id: "ghost".into(),
            session_cookie: "ghost-cookie".into(),
        });
        restore_secrets(&conn, &bundle).unwrap();

        assert_eq!(setting(&conn, "session_cookie").as_deref(), Some("SAPISID=active-secret"));
        assert_eq!(setting(&conn, "lastfm_session_key").as_deref(), Some("lfm-secret"));
        assert_eq!(setting(&conn, "listenbrainz_token").as_deref(), Some("lb-secret"));
        assert_eq!(
            setting(&conn, "proxy").as_deref(),
            Some("http://user:hunter2@proxy.local:3128")
        );
        let c: String = conn
            .query_row("SELECT session_cookie FROM accounts WHERE id = 'acc2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(c, "cookie-two");
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM accounts"), 2, "no row invented for 'ghost'");
    }

    #[test]
    fn profile_scrub_drop_signed_out_leaves_a_clean_signed_out_app() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        conn.execute("INSERT INTO playlist_track(playlist_id, video_id) VALUES('PL', 'v')", [])
            .unwrap();
        scrub(&conn, true).unwrap();
        drop_signed_out_accounts(&conn).unwrap();

        assert_eq!(n(&conn, "SELECT COUNT(*) FROM accounts"), 0);
        for key in IDENTITY_KEYS {
            assert_eq!(setting(&conn, key), None, "{key}");
        }
        assert_eq!(n(&conn, "SELECT COUNT(*) FROM playlist_track"), 0);
        assert_eq!(setting(&conn, "visitor_data").as_deref(), Some("visitor-keep"));
        assert_eq!(setting(&conn, "theme").as_deref(), Some("dark"));
    }

    #[test]
    fn profile_scrub_drop_signed_out_keeps_a_restored_session() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        let mut bundle = scrub(&conn, true).unwrap();
        // Only acc1 and the active cookie come back; acc2 stays blank and is dropped.
        bundle.google_sessions.retain(|s| s.account_id == "acc1");
        restore_secrets(&conn, &bundle).unwrap();
        drop_signed_out_accounts(&conn).unwrap();

        assert_eq!(n(&conn, "SELECT COUNT(*) FROM accounts"), 1);
        assert_eq!(setting(&conn, "session_cookie").as_deref(), Some("SAPISID=active-secret"));
        assert_eq!(setting(&conn, "active_account").as_deref(), Some("acc1"));
        assert_eq!(setting(&conn, "data_sync_id").as_deref(), Some("dsid1"));
    }

    #[test]
    fn profile_scrub_counts_and_channel_ids() {
        let dir = tempfile::tempdir().unwrap();
        let live = fixture(dir.path());
        let conn = Connection::open(&live).unwrap();
        conn.execute_batch(
            "INSERT INTO ytdata_accounts(channel_id, title, added_at) VALUES('UCb', 'B', 2), ('UCa', 'A', 1);",
        )
        .unwrap();
        let c = counts(&conn).unwrap();
        assert_eq!(c.accounts, 2);
        assert_eq!(c.ytdata_accounts, 2);
        assert_eq!(c.settings as i64, n(&conn, "SELECT COUNT(*) FROM settings"));
        assert!(c.settings >= 21);
        assert_eq!(c.files, 0);
        assert_eq!(ytdata_channel_ids(&conn).unwrap(), vec!["UCa".to_string(), "UCb".to_string()]);
    }

    #[test]
    fn profile_scrub_tolerates_an_old_schema_with_missing_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO settings VALUES('session_cookie', 'c'), ('theme', 'x');",
        )
        .unwrap();
        let c = counts(&conn).unwrap();
        assert_eq!(c, Counts { settings: 2, ..Counts::default() });
        assert!(ytdata_channel_ids(&conn).unwrap().is_empty());
        let bundle = scrub(&conn, false).unwrap();
        assert_eq!(bundle.active_session_cookie.as_deref(), Some("c"));
        drop_signed_out_accounts(&conn).unwrap();
        restore_secrets(&conn, &bundle).unwrap();
        assert_eq!(setting(&conn, "session_cookie").as_deref(), Some("c"));
    }
}
