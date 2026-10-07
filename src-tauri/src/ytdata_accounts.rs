//! `ytdata_accounts`: the YouTube channels connected through the Data API (OAuth), apart from
//! the cookie accounts in `accounts`. Metadata only; refresh tokens live in the OS keyring or the
//! DPAPI file (`ytdata_secrets`).
//!
//! `linked_account` optionally names the cookie account (`accounts.id`) that is the same Google
//! login, so the UI can pair them. Removing a row leaves its jobs in place, unowned
//! (`jobs.account_id ON DELETE SET NULL`), and its quota ledger rows untouched.
// used by commit 25+ (account manager wiring, settings and the jobs runner).
#![allow(dead_code)]

use std::sync::Arc;

use rusqlite::{params, OptionalExtension, Row};
use ytdata::auth::accounts::{Account, AccountStatus, AccountsRepo};

use crate::db::Db;

/// One `ytdata_accounts` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDataAccount {
    pub channel_id: String,
    pub title: String,
    pub thumb: Option<String>,
    pub status: AccountStatus,
    /// Unix seconds.
    pub added_at: i64,
    pub linked_account: Option<String>,
}

const SELECT: &str =
    "SELECT channel_id, title, thumb, status, added_at, linked_account FROM ytdata_accounts";

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<YtDataAccount> {
    let status: String = row.get(3)?;
    Ok(YtDataAccount {
        channel_id: row.get(0)?,
        title: row.get(1)?,
        thumb: row.get(2)?,
        // The column's CHECK allows nothing else; asking to reconnect is the safe reading anyway.
        status: AccountStatus::parse(&status).unwrap_or(AccountStatus::ReauthRequired),
        added_at: row.get(4)?,
        linked_account: row.get(5)?,
    })
}

/// Every connected channel, in the order they were added.
pub fn list(db: &Db) -> rusqlite::Result<Vec<YtDataAccount>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY added_at, rowid"))?;
    let rows = stmt.query_map([], row_to_account)?;
    rows.collect()
}

pub fn get(db: &Db, channel_id: &str) -> rusqlite::Result<Option<YtDataAccount>> {
    db.conn()
        .query_row(&format!("{SELECT} WHERE channel_id = ?1"), [channel_id], row_to_account)
        .optional()
}

/// Adds a channel as `connected`, or refreshes its title and thumbnail and marks it `connected`
/// again (a reconnect). `added_at` and `linked_account` of an existing row are kept.
pub fn upsert(
    db: &Db,
    channel_id: &str,
    title: &str,
    thumb: Option<&str>,
    added_at: i64,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO ytdata_accounts (channel_id, title, thumb, status, added_at) \
         VALUES (?1, ?2, ?3, 'connected', ?4) \
         ON CONFLICT(channel_id) DO UPDATE SET \
             title = excluded.title, thumb = excluded.thumb, status = 'connected'",
        params![channel_id, title, thumb, added_at],
    )?;
    Ok(())
}

/// Returns whether the channel exists.
pub fn set_status(db: &Db, channel_id: &str, status: AccountStatus) -> rusqlite::Result<bool> {
    let changed = db.conn().execute(
        "UPDATE ytdata_accounts SET status = ?2 WHERE channel_id = ?1",
        params![channel_id, status.as_str()],
    )?;
    Ok(changed > 0)
}

/// Pairs the channel with a cookie account (`accounts.id`), or unpairs it with `None`. Returns
/// whether the channel exists.
pub fn set_linked_account(
    db: &Db,
    channel_id: &str,
    linked_account: Option<&str>,
) -> rusqlite::Result<bool> {
    let changed = db.conn().execute(
        "UPDATE ytdata_accounts SET linked_account = ?2 WHERE channel_id = ?1",
        params![channel_id, linked_account],
    )?;
    Ok(changed > 0)
}

/// Removes the channel's row. Its jobs stay, unowned; its ledger rows stay as they are. Returns
/// whether it existed.
pub fn remove(db: &Db, channel_id: &str) -> rusqlite::Result<bool> {
    let removed =
        db.conn().execute("DELETE FROM ytdata_accounts WHERE channel_id = ?1", [channel_id])?;
    Ok(removed > 0)
}

fn store_error(e: rusqlite::Error) -> ytdata::error::Error {
    ytdata::error::Error::Io(std::io::Error::other(format!("ytdata_accounts: {e}")))
}

/// [`AccountsRepo`] over this table, for `ytdata`'s `AccountManager`. `save` hands over the whole
/// list: rows in it are upserted with the manager's status, rows missing from it are removed,
/// all in one transaction. `linked_account` is the app's own column and survives a save.
pub struct DbAccountsRepo {
    db: Arc<Db>,
}

impl DbAccountsRepo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

impl AccountsRepo for DbAccountsRepo {
    fn load(&self) -> Result<Vec<Account>, ytdata::error::Error> {
        let rows = list(&self.db).map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|a| Account {
                channel_id: a.channel_id,
                title: a.title,
                thumbnail_url: a.thumb,
                status: a.status,
                added_at_unix: u64::try_from(a.added_at).unwrap_or(0),
            })
            .collect())
    }

    fn save(&self, accounts: &[Account]) -> Result<(), ytdata::error::Error> {
        replace_all(&self.db, accounts).map_err(store_error)
    }
}

/// [`DbAccountsRepo::save`]'s transaction.
fn replace_all(db: &Db, accounts: &[Account]) -> rusqlite::Result<()> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    {
        let mut upsert = tx.prepare(
            "INSERT INTO ytdata_accounts (channel_id, title, thumb, status, added_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(channel_id) DO UPDATE SET \
                 title = excluded.title, thumb = excluded.thumb, status = excluded.status",
        )?;
        for a in accounts {
            upsert.execute(params![
                a.channel_id,
                a.title,
                a.thumbnail_url,
                a.status.as_str(),
                i64::try_from(a.added_at_unix).unwrap_or(i64::MAX),
            ])?;
        }
        let kept: Vec<&str> = accounts.iter().map(|a| a.channel_id.as_str()).collect();
        let stored: Vec<String> = {
            let mut stmt = tx.prepare("SELECT channel_id FROM ytdata_accounts")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for gone in stored.iter().filter(|id| !kept.contains(&id.as_str())) {
            tx.execute("DELETE FROM ytdata_accounts WHERE channel_id = ?1", [gone])?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn account(id: &str, status: AccountStatus, added_at: u64) -> Account {
        Account {
            channel_id: id.into(),
            title: format!("Channel {id}"),
            thumbnail_url: Some(format!("https://yt3/{id}.jpg")),
            status,
            added_at_unix: added_at,
        }
    }

    #[test]
    fn upsert_adds_then_refreshes_without_losing_the_original_date_or_link() {
        let db = db();
        upsert(&db, "UC1", "Old", None, 100).unwrap();
        assert!(set_linked_account(&db, "UC1", Some("ga-abc")).unwrap());
        assert!(set_status(&db, "UC1", AccountStatus::ReauthRequired).unwrap());

        upsert(&db, "UC1", "New", Some("https://yt3/1.jpg"), 999).unwrap();
        let a = get(&db, "UC1").unwrap().unwrap();
        assert_eq!(
            a,
            YtDataAccount {
                channel_id: "UC1".into(),
                title: "New".into(),
                thumb: Some("https://yt3/1.jpg".into()),
                status: AccountStatus::Connected,
                added_at: 100,
                linked_account: Some("ga-abc".into()),
            },
            "a reconnect refreshes the metadata and the status, nothing else"
        );
        assert!(set_linked_account(&db, "UC1", None).unwrap());
        assert_eq!(get(&db, "UC1").unwrap().unwrap().linked_account, None);
    }

    #[test]
    fn list_is_in_the_order_added_and_unknown_channels_report_false() {
        let db = db();
        upsert(&db, "UC-b", "B", None, 200).unwrap();
        upsert(&db, "UC-a", "A", None, 100).unwrap();
        let ids: Vec<String> = list(&db).unwrap().into_iter().map(|a| a.channel_id).collect();
        assert_eq!(ids, ["UC-a", "UC-b"]);
        assert_eq!(get(&db, "UC-x").unwrap(), None);
        assert!(!set_status(&db, "UC-x", AccountStatus::Connected).unwrap());
        assert!(!set_linked_account(&db, "UC-x", Some("ga")).unwrap());
        assert!(!remove(&db, "UC-x").unwrap());
        assert!(remove(&db, "UC-a").unwrap());
        assert_eq!(list(&db).unwrap().len(), 1);
    }

    #[test]
    fn removing_an_account_unowns_its_jobs_and_keeps_the_ledger() {
        let db = db();
        upsert(&db, "UC1", "Me", None, 1).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO jobs(id, account_id, kind, created_at) VALUES(1, 'UC1', 'k', 't');
                 INSERT INTO quota_ledger(ts, endpoint, units, account_id, job_id)
                     VALUES('t', 'playlists.insert', 50, 'UC1', 1);",
            )
            .unwrap();
        assert!(remove(&db, "UC1").unwrap());
        let conn = db.conn();
        let owner: Option<String> =
            conn.query_row("SELECT account_id FROM jobs WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(owner, None);
        let spent: (i64, Option<String>) = conn
            .query_row("SELECT units, account_id FROM quota_ledger", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(spent, (50, Some("UC1".into())));
    }

    #[test]
    fn db_accounts_repo_round_trips_and_removes_what_the_manager_dropped() {
        let db = Arc::new(db());
        let repo = DbAccountsRepo::new(db.clone());
        assert_eq!(repo.load().unwrap(), Vec::new());

        let first = vec![
            account("UC1", AccountStatus::Connected, 100),
            account("UC2", AccountStatus::ReauthRequired, 200),
        ];
        repo.save(&first).unwrap();
        assert_eq!(repo.load().unwrap(), first);

        set_linked_account(&db, "UC1", Some("ga-1")).unwrap();
        let second = vec![account("UC1", AccountStatus::ReauthRequired, 100)];
        repo.save(&second).unwrap();
        assert_eq!(repo.load().unwrap(), second, "UC2 removed, UC1's status saved");
        let kept = get(&db, "UC1").unwrap().unwrap();
        assert_eq!(kept.linked_account.as_deref(), Some("ga-1"), "the app's column survives");

        repo.save(&[]).unwrap();
        assert!(repo.load().unwrap().is_empty());
    }

    #[test]
    fn the_account_manager_loads_through_the_db_repo() {
        let db = Arc::new(db());
        upsert(&db, "UC1", "Me", None, 100).unwrap();
        set_status(&db, "UC1", AccountStatus::ReauthRequired).unwrap();
        let store: Arc<dyn ytdata::auth::store::TokenStore> =
            Arc::new(ytdata::auth::store::InMemoryTokenStore::default());
        let manager =
            ytdata::auth::accounts::AccountManager::new(store, Arc::new(DbAccountsRepo::new(db)))
                .unwrap();
        assert_eq!(manager.account("UC1").unwrap().status, AccountStatus::ReauthRequired);
    }
}
