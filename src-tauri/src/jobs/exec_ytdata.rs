//! The Data API engine: one `PlannedAction` per item, through `ytdata`. Port of the execution half
//! of PlaylistForge's `pf-app/src/jobs/runner.rs`.
//!
//! The client here has **no** quota sink: a write's units are billed by the runner in the same
//! transaction as its item's `done` (`repo::complete_item`), and a sink would bill them a second
//! time. The reads this module makes on a job's behalf (resolving deletes, reconciling) are billed
//! explicitly, with the job's id.
//!
//! The token store blocks (the OS keyring, a DPAPI file), so tokens are fetched on a blocking
//! thread. A 401 refreshes the token and retries the call once.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use ytdata::auth::accounts::AccountManager;
use ytdata::client::YouTubeClient;
use ytdata::error::{ApiErrorKind, AuthError, Error as YtError};
use ytdata::types::{PlaylistItemSummary, PlaylistSummary};

use super::planner::{self, action_name, PlannedAction};
use super::runner::{Barrier, Executor, ItemOutcome, Reconcile};
use super::{local_echo, repo, Job, JobItem, JobItemStatus};
use crate::db::Db;
use crate::quota::{self, endpoint};

/// The app's account manager, set once the client secret is known (and replaced on import).
pub type SharedAccounts = Arc<std::sync::RwLock<Option<Arc<AccountManager>>>>;

/// A playlist's items, at most this many pages (10 000 rows, twice YouTube's cap), so a runaway
/// `nextPageToken` can't loop forever.
const MAX_ITEM_PAGES: i64 = 200;
const MAX_PLAYLIST_PAGES: i64 = 50;

pub struct YtDataExecutor {
    db: Arc<Db>,
    accounts: SharedAccounts,
    client: Option<Arc<YouTubeClient>>,
}

impl YtDataExecutor {
    pub fn new(db: Arc<Db>, accounts: SharedAccounts) -> Self {
        let client = match YouTubeClient::new() {
            Ok(client) => Some(Arc::new(client)),
            Err(e) => {
                tracing::warn!(error = %e, "jobs: could not build the Data API client");
                None
            }
        };
        Self { db, accounts, client }
    }

    fn manager(&self) -> Option<Arc<AccountManager>> {
        self.accounts.read().ok()?.clone()
    }

    /// A valid access token, fetched on a blocking thread. `fresh` drops the cached one first.
    async fn token(&self, channel_id: &str, fresh: bool) -> Result<String, YtError> {
        let Some(manager) = self.manager() else {
            return Err(AuthError::NoClientSecret.into());
        };
        let id = channel_id.to_string();
        let joined = tokio::task::spawn_blocking(move || {
            if fresh {
                manager.invalidate_cached_token(&id);
            }
            tokio::runtime::Handle::current().block_on(manager.get_valid_access_token(&id))
        })
        .await;
        joined.unwrap_or_else(|e| Err(YtError::Io(std::io::Error::other(e.to_string()))))
    }

    /// Runs `call` with a token; on a 401, once more with a fresh one.
    async fn call<T, F, Fut>(&self, channel_id: &str, call: F) -> Result<T, YtError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, YtError>>,
    {
        let token = self.token(channel_id, false).await?;
        match call(token).await {
            Err(e) if e.is_unauthorized() => call(self.token(channel_id, true).await?).await,
            other => other,
        }
    }

    /// Every item of a playlist, and how many pages (units) that took.
    async fn list_items(
        &self,
        client: &YouTubeClient,
        channel_id: &str,
        playlist_id: &str,
    ) -> Result<(Vec<PlaylistItemSummary>, i64), YtError> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages = 0;
        while pages < MAX_ITEM_PAGES {
            let current = page_token.take();
            let page = self
                .call(channel_id, move |token| {
                    let current = current.clone();
                    async move {
                        let at = current.as_deref();
                        client.playlist_items_list(&token, channel_id, playlist_id, at).await
                    }
                })
                .await?;
            pages += 1;
            all.extend(page.items);
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        Ok((all, pages))
    }

    async fn list_playlists(
        &self,
        client: &YouTubeClient,
        channel_id: &str,
    ) -> Result<(Vec<PlaylistSummary>, i64), YtError> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages = 0;
        while pages < MAX_PLAYLIST_PAGES {
            let current = page_token.take();
            let page = self
                .call(channel_id, move |token| {
                    let current = current.clone();
                    async move {
                        client.playlists_list_mine(&token, channel_id, current.as_deref()).await
                    }
                })
                .await?;
            pages += 1;
            all.extend(page.items);
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        Ok((all, pages))
    }

    /// A read made for a job, billed with it.
    fn bill_read(&self, endpoint_name: &str, pages: i64, channel_id: &str, job_id: i64) {
        if pages <= 0 {
            return;
        }
        let at = Utc::now();
        if let Err(e) =
            quota::record_units(&self.db, endpoint_name, pages, Some(channel_id), Some(job_id), at)
        {
            tracing::warn!(error = %e, "jobs: could not record a read's quota");
        }
    }

    /// The barrier: what the destination holds now.
    async fn verify(&self, client: &YouTubeClient, job: &Job, channel_id: &str) -> ItemOutcome {
        let dest = job.params.get("dest_playlist_id").and_then(Value::as_str).unwrap_or_default();
        match self.list_items(client, channel_id, dest).await {
            Ok((items, pages)) => ItemOutcome::Verified {
                present: items.into_iter().map(|i| i.video_id).collect::<HashSet<_>>(),
                units: pages,
            },
            Err(e) if e.api_error_kind() == Some(ApiErrorKind::ItemGone) => {
                ItemOutcome::FailJob("the destination playlist is gone".into())
            }
            Err(e) => classify(&e),
        }
    }

    /// Resolves every unresolved delete of this job on this playlist with one listing (see
    /// `planner`'s module doc), and answers this item's resolved params. `Ok(None)`: its row is
    /// no longer in the playlist. A sibling whose row is gone is marked so, to be skipped
    /// without another listing.
    async fn resolve_deletes(
        &self,
        client: &YouTubeClient,
        job: &Job,
        item: &JobItem,
        channel_id: &str,
    ) -> Result<Option<Value>, ItemOutcome> {
        let playlist = item.params.get("playlist_id").and_then(Value::as_str).unwrap_or_default();
        let (listed, pages) = match self.list_items(client, channel_id, playlist).await {
            Ok(listed) => listed,
            Err(e) if e.api_error_kind() == Some(ApiErrorKind::ItemGone) => {
                return Err(ItemOutcome::Skipped("the playlist is gone".into()));
            }
            Err(e) => return Err(classify(&e)),
        };
        self.bill_read(endpoint::PLAYLIST_ITEMS_LIST, pages, channel_id, job.id);
        let rows: Vec<(String, String, i64)> =
            listed.into_iter().map(|i| (i.video_id, i.playlist_item_id, i.position)).collect();
        let siblings: Vec<JobItem> = repo::list_job_items(&self.db, job.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|i| matches!(i.status, JobItemStatus::Pending | JobItemStatus::InFlight))
            .filter(|i| planner::is_unresolved_delete(i))
            .filter(|i| i.params.get("playlist_id").and_then(Value::as_str) == Some(playlist))
            .collect();
        let resolved = planner::resolve_occurrences(&rows, &siblings);
        let mut mine = None;
        for sibling in &siblings {
            let params = match resolved.iter().find(|(id, _)| *id == sibling.id) {
                Some((_, params)) => params.clone(),
                None => super::set_param(sibling.params.clone(), "gone", json!(true)),
            };
            if sibling.id == item.id {
                mine = Some(params.clone());
            }
            if let Err(e) = repo::set_item_params(&self.db, sibling.id, &params) {
                tracing::warn!(error = %e, "jobs: could not keep a resolved delete");
            }
        }
        Ok(mine.filter(|p| p.get("playlist_item_id").is_some()))
    }
}

/// Makes one write. Answers its result, its cost and the endpoint it bills.
async fn execute_planned_action(
    client: &YouTubeClient,
    token: String,
    account_id: &str,
    action: &PlannedAction,
) -> Result<(Value, i64, &'static str), YtError> {
    let token = token.as_str();
    let result = match action {
        PlannedAction::PlaylistInsert { title, description, privacy } => {
            let id =
                client.playlists_insert(token, account_id, title, description, privacy).await?;
            json!({ "id": id })
        }
        PlannedAction::PlaylistUpdate { playlist_id, title, description, privacy, .. } => {
            client
                .playlists_update(token, account_id, playlist_id, title, description, privacy)
                .await?;
            json!({})
        }
        PlannedAction::PlaylistDelete { playlist_id } => {
            client.playlists_delete(token, account_id, playlist_id).await?;
            json!({})
        }
        PlannedAction::PlaylistItemInsert { playlist_id, video_id, position } => {
            let position = position.map(|p| p.max(0) as u32);
            let written = client
                .playlist_items_insert(token, account_id, playlist_id, video_id, position)
                .await?;
            json!({"playlist_item_id": written.playlist_item_id, "position": written.position})
        }
        PlannedAction::PlaylistItemUpdatePosition {
            playlist_item_id,
            playlist_id,
            video_id,
            position,
            ..
        } => {
            let written = client
                .playlist_items_update_position(
                    token,
                    account_id,
                    playlist_item_id,
                    playlist_id,
                    video_id,
                    (*position).max(0) as u32,
                )
                .await?;
            json!({"playlist_item_id": written.playlist_item_id, "position": written.position})
        }
        PlannedAction::PlaylistItemDelete { playlist_item_id, .. } => {
            client.playlist_items_delete(token, account_id, playlist_item_id).await?;
            json!({})
        }
    };
    Ok((result, action.cost(), action.endpoint()))
}

/// What a failed call means for the job.
pub fn classify(err: &YtError) -> ItemOutcome {
    let auth = match err {
        YtError::Auth(AuthError::InvalidGrant) => Some("invalid_grant"),
        YtError::Auth(AuthError::NoRefreshToken) => Some("no_refresh_token"),
        YtError::Auth(AuthError::SecretUndecryptable) => Some("secret_undecryptable"),
        YtError::Auth(AuthError::NoClientSecret) => Some("no_client_secret"),
        YtError::Auth(AuthError::KeyringUnavailable(_)) => Some("keyring_unavailable"),
        _ => None,
    };
    if let Some(reason) = auth {
        return ItemOutcome::NeedsAuth(reason.into());
    }
    match err.api_error_kind() {
        Some(ApiErrorKind::QuotaExceeded) => ItemOutcome::QuotaExceeded(err.to_string()),
        Some(ApiErrorKind::ApiDisabled) => ItemOutcome::NeedsAuth("api_disabled".into()),
        Some(ApiErrorKind::ManualSortRequired) => ItemOutcome::FailJob(
            "manualSortRequired: the playlist is not in manual sort order".into(),
        ),
        Some(ApiErrorKind::ItemGone) => ItemOutcome::Skipped(err.to_string()),
        // Rate limits, 5xx after the client's own retries, the network: back off.
        _ => ItemOutcome::Retry(err.to_string()),
    }
}

impl Executor for YtDataExecutor {
    async fn execute(&self, job: &Job, item: &JobItem) -> ItemOutcome {
        let Some(channel) = job.account_id.clone() else {
            return ItemOutcome::NeedsAuth("no_account_for_job".into());
        };
        let Some(client) = self.client.clone() else {
            return ItemOutcome::Retry("the Data API client is unavailable".into());
        };
        if item.action == action_name::VERIFY_DESTINATION {
            return self.verify(&client, job, &channel).await;
        }
        let mut params = item.params.clone();
        if params.get("gone").and_then(Value::as_bool) == Some(true) {
            return ItemOutcome::Skipped("no longer in the playlist".into());
        }
        if planner::is_unresolved_delete(item) {
            match self.resolve_deletes(&client, job, item, &channel).await {
                Ok(Some(resolved)) => params = resolved,
                Ok(None) => return ItemOutcome::Skipped("no longer in the playlist".into()),
                Err(outcome) => return outcome,
            }
        }
        let Some(action) = PlannedAction::from_params(&item.action, &params) else {
            return ItemOutcome::Failed(format!("unrecognized action {}", item.action));
        };
        let (client, channel, action_ref) = (client.as_ref(), channel.as_str(), &action);
        let result = self
            .call(channel, move |token| execute_planned_action(client, token, channel, action_ref))
            .await;
        // What a write learns about the API as a whole (disabled, out of quota, enabled again)
        // feeds the same marks the warnings and the engine choice read (ytdata_status.rs).
        match result {
            Ok((api_result, units, endpoint)) => {
                crate::ytdata_status::note_success(&self.db);
                ItemOutcome::Done {
                    inverse: action.inverse_json(Some(&api_result)),
                    api_result,
                    units,
                    endpoint: Some(endpoint),
                }
            }
            Err(e) => {
                crate::ytdata_status::note_error(&self.db, &e, Utc::now());
                classify(&e)
            }
        }
    }

    async fn reconcile(&self, job: &Job, item: &JobItem) -> Reconcile {
        let is_item_insert = item.action == action_name::PLAYLIST_ITEM_INSERT;
        if !is_item_insert && item.action != action_name::PLAYLIST_INSERT {
            // Updates, deletes and the barrier are safe to run again.
            return Reconcile::Requeue("interrupted; safe to repeat".into());
        }
        let Some(action) = PlannedAction::from_params(&item.action, &item.params) else {
            return Reconcile::Requeue("interrupted".into());
        };
        let (Some(channel), Some(client)) = (job.account_id.as_deref(), self.client.clone()) else {
            return Reconcile::LeaveAuth("no_account_for_job".into());
        };
        // An insert may have landed before the crash: look before sending it again.
        let found = match &action {
            PlannedAction::PlaylistItemInsert { playlist_id, video_id, .. } => {
                match self.list_items(&client, channel, playlist_id).await {
                    Ok((items, pages)) => {
                        self.bill_read(endpoint::PLAYLIST_ITEMS_LIST, pages, channel, job.id);
                        items.into_iter().find(|i| &i.video_id == video_id).map(|i| {
                            json!({"playlist_item_id": i.playlist_item_id, "position": i.position})
                        })
                    }
                    Err(e) if e.needs_reauthorization() => {
                        return Reconcile::LeaveAuth(e.to_string())
                    }
                    Err(e) => return Reconcile::Leave(e.to_string()),
                }
            }
            PlannedAction::PlaylistInsert { title, .. } => {
                match self.list_playlists(&client, channel).await {
                    Ok((playlists, pages)) => {
                        self.bill_read(endpoint::PLAYLISTS_LIST, pages, channel, job.id);
                        let found = playlists.into_iter().find(|p| &p.title == title);
                        found.map(|p| json!({ "id": p.id }))
                    }
                    Err(e) if e.needs_reauthorization() => {
                        return Reconcile::LeaveAuth(e.to_string())
                    }
                    Err(e) => return Reconcile::Leave(e.to_string()),
                }
            }
            _ => None,
        };
        match found {
            Some(api_result) => Reconcile::Done {
                inverse: action.inverse_json(Some(&api_result)),
                api_result,
                units: action.cost(),
                endpoint: Some(action.endpoint()),
            },
            None => Reconcile::Requeue("the interrupted write never landed; sending again".into()),
        }
    }

    fn after_verify(&self, job: &Job, phase1: &[JobItem], present: &HashSet<String>) -> Barrier {
        planner::ytdata_move_barrier(job, phase1, present)
    }

    fn after_done(&self, _job: &Job, item: &JobItem, _api_result: &Value) {
        // Re-read: a delete's params were completed while it ran.
        let params = repo::get_job_item(&self.db, item.id)
            .ok()
            .flatten()
            .map(|i| i.params)
            .unwrap_or_else(|| item.params.clone());
        if let Some(action) = PlannedAction::from_params(&item.action, &params) {
            local_echo::echo_completed_action(&self.db, &action, Utc::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, reason: Option<&str>) -> YtError {
        YtError::Api { status, reason: reason.map(Into::into), message: "x".into() }
    }

    #[test]
    fn failures_map_to_what_the_job_does_next() {
        assert_eq!(
            classify(&YtError::Auth(AuthError::InvalidGrant)),
            ItemOutcome::NeedsAuth("invalid_grant".into())
        );
        assert_eq!(
            classify(&YtError::Auth(AuthError::KeyringUnavailable("no dbus".into()))),
            ItemOutcome::NeedsAuth("keyring_unavailable".into())
        );
        assert!(matches!(
            classify(&api(403, Some("quotaExceeded"))),
            ItemOutcome::QuotaExceeded(_)
        ));
        assert_eq!(
            classify(&api(403, Some("accessNotConfigured"))),
            ItemOutcome::NeedsAuth("api_disabled".into())
        );
        assert!(matches!(classify(&api(400, Some("manualSortRequired"))), ItemOutcome::FailJob(_)));
        assert!(matches!(classify(&api(404, None)), ItemOutcome::Skipped(_)));
        assert!(matches!(classify(&api(429, None)), ItemOutcome::Retry(_)));
        assert!(matches!(classify(&api(503, None)), ItemOutcome::Retry(_)));
    }

    #[tokio::test]
    async fn without_an_account_manager_a_job_waits_for_the_user() {
        let db = Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap());
        let exec = YtDataExecutor::new(db, Arc::new(std::sync::RwLock::new(None)));
        let token = exec.token("UC1", false).await;
        assert!(matches!(token, Err(YtError::Auth(AuthError::NoClientSecret))));
        let waiting = ItemOutcome::NeedsAuth("no_client_secret".into());
        assert_eq!(classify(&token.unwrap_err()), waiting);
    }
}
